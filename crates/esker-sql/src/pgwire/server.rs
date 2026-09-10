//! The socket edge, and the only place in this crate that knows what a socket is.
//!
//! Everything else — the codec, the session, the parser, the backend seam — is synchronous and
//! testable without a listener. This module's whole job is to turn a byte stream into framed
//! messages, hand them to a [`Session`], and write back what it produces. It is deliberately thin,
//! because logic that lives here can only be tested through a socket.
//!
//! [`Connection`] is generic over the stream rather than tied to `TcpStream`, so the whole startup
//! handshake and message loop can be driven over an in-memory pipe in a unit test. `TcpListener`
//! appears in exactly one function.

use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::backend::Backend;
use crate::error::{Result, Severity, SqlError};
use crate::pgwire::message::{
    Backend as Message, MAX_MESSAGE_LEN, PROTOCOL_MINOR, Startup, TransactionStatus, decode,
    decode_startup,
};
use crate::pgwire::session::{Execute, Session};
use crate::pgwire::tls::{MaybeTlsStream, TlsConfig};
use crate::pgwire::{Negotiation, error_fields, error_message, negotiation};

/// How a connecting client proves who it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Auth {
    /// Anyone who can reach the socket may connect. The default, and only sane behind a trusted
    /// network boundary.
    #[default]
    Trust,
    /// The client sends a password in the clear, which is only acceptable over a trusted link —
    /// this node terminates no TLS, so the password really is in the clear.
    Cleartext {
        /// The password every user must send. A single shared secret is all phase 6a needs; real
        /// per-role credentials are a catalog concern and come later.
        expected: &'static str,
    },
}

/// What a listener needs to know.
#[derive(Debug, Clone)]
pub struct Config {
    /// Address to listen on.
    pub address: String,
    /// How clients authenticate.
    pub auth: Auth,
    /// Reported to the client as `server_version`. PostgreSQL clients parse this and change
    /// behaviour on it, so it names a real PostgreSQL version and then says what is actually here.
    pub server_version: String,
    /// Whether this node terminates TLS, and what with.
    ///
    /// [`TlsConfig::disabled`] is the default and answers `SSLRequest` with `N`, which is what
    /// every build without the `tls` feature can do. Building an enabled one is a startup-time
    /// decision the binary makes ([`TlsConfig::from_pem_files`]), never a per-connection one.
    pub tls: TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            address: "127.0.0.1:5432".to_owned(),
            auth: Auth::Trust,
            server_version: "19.0 (Esker)".to_owned(),
            tls: TlsConfig::disabled(),
        }
    }
}

/// Builds the executor for one connection.
///
/// A trait rather than a closure because it is shared across connections and each one needs its own
/// executor: sessions do not share portals, prepared statements or transactions.
pub trait Executors: Send + Sync + 'static {
    /// Makes an executor for one new session, serving the database the startup packet named.
    ///
    /// # Errors
    ///
    /// `3D000` when the cluster has no such database, which is the answer a real server gives and
    /// the one `rake db:create` reads to know it must create one.
    /// `identity` is **the session this executor is**, and it is a parameter rather than
    /// something the implementor makes for itself.
    ///
    /// The connection has already announced this pid and key in `BackendKeyData`, so an
    /// implementor that registered its own would give the executor an identity no client was ever
    /// told — a `CancelRequest` would name the announced one and reach nobody, and
    /// `pg_stat_activity` would show a session the client cannot cancel. Passing it in is what
    /// makes those the same session.
    fn for_session(
        &self,
        database: &str,
        identity: crate::session::Backend,
    ) -> Result<Box<dyn Execute + Send>>;
}

/// Accepts connections until the process ends.
///
/// # Errors
///
/// Fails if the address cannot be bound. A failure on one accepted connection is logged and does
/// not stop the listener: one client's broken pipe is not the server's problem.
pub async fn serve(config: Config, executors: Arc<dyn Executors>) -> std::io::Result<()> {
    let listener = TcpListener::bind(&config.address).await?;
    let bound = listener.local_addr()?;
    tracing::info!(address = %bound, "esker-sql is listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        let config = config.clone();
        let executors = Arc::clone(&executors);
        tokio::spawn(async move {
            // Nagle would add a round trip's worth of latency to every small reply, and almost
            // every reply in this protocol is small.
            let _ = stream.set_nodelay(true);
            if let Err(error) = accept(stream, peer, config, Arc::clone(&executors)).await {
                tracing::debug!(%peer, %error, "connection ended");
            }
        });
    }
}

impl<S> Drop for Connection<S> {
    /// **Forgets this session however the connection ends**, which is why it is a destructor and
    /// not a line at each `return`: the run loop leaves by many routes — `Terminate`, the client
    /// vanishing, an idle-in-transaction timeout, `pg_terminate_backend` (twice: before a message
    /// is handled, and after one that terminated this session itself), a failed startup, an I/O
    /// error — and a registry that leaked one entry per dropped connection would grow for the life
    /// of the process and hand out pids that answer for nobody. The count is deliberately not
    /// given here: it was "six" and two more arrived.
    fn drop(&mut self) {
        if let Some(backend) = &self.backend {
            crate::session::deregister(backend.pid);
        }
    }
}

/// One client connection.
#[derive(Debug)]
pub struct Connection<S> {
    stream: S,
    config: Config,
    /// The role the client asked to connect as, for the authentication failure message.
    user: String,
    /// The database the startup packet asked for, which selects the tenant the session runs as
    /// ([ADR 0052](../../../../docs/adr/0052-a-database-is-a-tenant-and-the-directory-that-names-them.md)).
    ///
    /// **It defaults to the user's name**, which is PostgreSQL's rule and is what makes `psql`
    /// with no `-d` connect to a database named for whoever is running it.
    database: String,
    /// libpq's `options` startup parameter, verbatim.
    ///
    /// A command line — `-c name=value` and `--name=value` — applied to the session once it
    /// exists. Empty when the client sent none, which is every client that does not ask.
    options: String,
    /// Reused between messages so a busy session is not allocating a buffer per reply.
    ///
    /// It travels to the blocking thread with the session and comes back, so "reused" survives
    /// that trip — which is the point of moving the whole bundle rather than copying out of it.
    out: Vec<u8>,
    /// A startup packet the accept path read while answering `SSLRequest`, waiting to be decoded.
    ///
    /// `None` for a connection built directly, which is what the in-memory tests do.
    pending: Option<Vec<u8>>,
    /// The `application_name` the startup packet carried, or empty when it carried none.
    ///
    /// Kept beside `user` and `database` for the same reason they are: `pg_stat_activity` reports
    /// it, and the parameters are gone by the time a session exists (`debts-v1.1.md` #47).
    application_name: String,
    /// Who is on the other end, when there is a socket to ask.
    ///
    /// `None` for a connection built directly — every in-process test — which is exactly when
    /// PostgreSQL answers NULL for `client_addr` too.
    peer: Option<std::net::SocketAddr>,
    /// Bytes read off the socket **while a statement was running**, waiting to be framed.
    ///
    /// [`Connection::watch_for_the_client_leaving`] has to read to learn that the peer is gone —
    /// a socket has no other way to say so — and a client is entitled to pipeline while its
    /// statement runs, so whatever it sent has to be given back. Every read goes through
    /// [`Connection::read_exact_or_eof`], which drains this first, so the message stream is the
    /// same one the client wrote whether or not anybody was watching.
    spare: Vec<u8>,
    /// This session's pid, key and cancellation flag, from the moment startup completes.
    ///
    /// `None` before then and for a connection that never got that far — a `CancelRequest` is
    /// itself a connection that never completes a startup, and registering one would put a
    /// session in the table that can never run a statement.
    backend: Option<crate::session::Backend>,
}

/// What one message's worth of work owns while it runs.
///
/// It is a bundle because it has to **move**: everything below [`Execute`] is synchronous and
/// talks to the network, so a statement cannot run on a runtime thread
/// (`docs/plans/phase-6a.md` §5). Running it there was harmless while the store was in this
/// process and stops the node dead against a real one — `BlockingTransport::call was used inside
/// an async runtime`, once per statement, for every client.
struct Work {
    session: Session,
    executor: Box<dyn Execute + Send>,
    out: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Connection<S> {
    /// Wraps a stream that has not yet sent its startup packet.
    pub fn new(stream: S, config: Config) -> Self {
        Self::resuming(stream, config, None)
    }

    /// Wraps a stream whose first startup packet has already been read by the accept path.
    ///
    /// `pending` is that packet, when it was not an encryption request and so still has to be
    /// decoded by [`Connection::startup`].
    fn resuming(stream: S, config: Config, pending: Option<Vec<u8>>) -> Self {
        Connection {
            options: String::new(),
            stream,
            config,

            user: String::new(),
            database: String::new(),
            out: Vec::with_capacity(8 * 1024),
            backend: None,
            pending,
            spare: Vec::new(),
            application_name: String::new(),
            peer: None,
        }
    }

    /// Records the peer's address, for `pg_stat_activity` to report.
    ///
    /// Set by the accept path, which is where the socket's address is known: [`Connection`] is
    /// generic over its stream and a `TlsStream` has no `peer_addr` of its own.
    #[must_use]
    pub fn from_peer(mut self, peer: std::net::SocketAddr) -> Self {
        self.peer = Some(peer);
        self
    }

    /// Runs the connection to completion: startup, then messages until the client leaves.
    ///
    /// # Errors
    ///
    /// Any I/O failure. A *protocol* failure is reported to the client and ends the connection
    /// cleanly rather than surfacing here.
    pub async fn run(&mut self, executors: Arc<dyn Executors>) -> std::io::Result<()> {
        if !self.startup().await? {
            return Ok(());
        }
        let Some(executor) = self.open_session(executors).await? else {
            return Ok(());
        };
        let mut work = Work {
            session: Session::new(),
            executor,
            out: std::mem::take(&mut self.out),
        };
        loop {
            // **A session idling inside a transaction block is on a clock.** PostgreSQL's
            // `idle_in_transaction_session_timeout` does not cancel a statement, it **terminates
            // the session** — so it belongs here, around the wait for the client's next message,
            // and nowhere else: the executor is not running while this fires. Measured on the
            // oracle (`tests/corpus/pg19_transaction_timeouts.txt`): after it fires the next
            // statement is a connection-level `FATAL` and the `ROLLBACK` after that cannot find a
            // socket.
            //
            // A block that has *failed* is idling too — PostgreSQL reports it as "idle in
            // transaction (aborted)" and times it out the same way, which is why the test is
            // "not idle" rather than "in transaction".
            let waiting_in_a_block = work.session.status() != TransactionStatus::Idle;
            let deadline = waiting_in_a_block
                .then(|| work.executor.idle_in_transaction_timeout())
                .flatten();
            let next = match deadline {
                Some(limit) => match tokio::time::timeout(limit, self.read_message()).await {
                    Ok(next) => next?,
                    Err(_elapsed) => {
                        // The block is abandoned before the socket goes, so nothing it wrote is
                        // left half-open behind a connection nobody can reach any more.
                        //
                        // **On the blocking pool, for the same reason session creation is**:
                        // `rollback` is a Percolator rollback against the stores on a real
                        // cluster, so it reaches `BlockingTransport` exactly as the catalog read
                        // does. This is the second site the audit for that bug turned up; the
                        // `release_advisory_locks` beside it is in-process
                        // (`crate::advisory::Locks`) and needs no thread of its own.
                        let mut ending = work;
                        tokio::task::spawn_blocking(move || {
                            let _ = ending.executor.rollback();
                            ending.executor.release_advisory_locks();
                        })
                        .await
                        .map_err(std::io::Error::other)?;
                        self.send_error(&SqlError::IdleInTransactionTimeout).await?;
                        return Ok(());
                    }
                },
                None => self.read_message().await?,
            };
            let Some((tag, body)) = next else {
                // The client left. Its advisory locks go with it — they survive `ROLLBACK` and are
                // released by an explicit unlock or by the session ending, and this is the ending
                // (`crate::advisory`).
                work.executor.release_advisory_locks();
                return Ok(());
            };
            // **`pg_terminate_backend` ends the session, and this is where it lands.** The flag is
            // set by another connection's thread; this one owns the socket, so the close happens
            // here and before the message is handled — a terminated backend runs nothing else,
            // which is what makes it a termination rather than a very rude cancellation.
            //
            // The unwinding is the idle-in-transaction path's, for the same reason and on the same
            // pool: an open block is given back before the socket goes, so nothing it wrote is
            // left half-open behind a connection nobody can reach.
            if work.executor.terminated() {
                let mut ending = work;
                tokio::task::spawn_blocking(move || {
                    let _ = ending.executor.rollback();
                    ending.executor.release_advisory_locks();
                })
                .await
                .map_err(std::io::Error::other)?;
                self.send_error(&SqlError::TerminatedByAdministrator)
                    .await?;
                return Ok(());
            }
            let message = match decode(tag, &body) {
                Ok(message) => message,
                Err(error) => {
                    // A message we cannot even decode is not something the session can be asked
                    // about; it is reported and the connection ends, which is what PostgreSQL does
                    // with a protocol violation.
                    self.send_error(&error).await?;
                    return Ok(());
                }
            };
            if matches!(message, crate::pgwire::message::Frontend::Terminate) {
                work.executor.release_advisory_locks();
                return Ok(());
            }
            // Onto a blocking thread and back. `spawn_blocking` rather than `block_in_place`
            // because a node serves many connections at once: blocking a *worker* thread per
            // statement would starve the runtime of the threads it needs to read the next
            // message, where the blocking pool exists to be blocked.
            work = self.handle_while_watching(work, message).await?;
            // **A session may terminate itself**, and then its own answer must not be sent:
            // `SELECT pg_terminate_backend(pg_backend_pid())` on PostgreSQL replies `FATAL` and
            // closes — the `t` the function computed never reaches the client. The check before
            // the message is handled cannot see this one, because the flag is set *by* the
            // statement being handled.
            //
            // It also tightens the cross-session case: a victim terminated while its statement was
            // running is told so when that statement ends, rather than answering once more first.
            if work.executor.terminated() {
                let mut ending = work;
                tokio::task::spawn_blocking(move || {
                    let _ = ending.executor.rollback();
                    ending.executor.release_advisory_locks();
                })
                .await
                .map_err(std::io::Error::other)?;
                self.send_error(&SqlError::TerminatedByAdministrator)
                    .await?;
                return Ok(());
            }
            if !work.out.is_empty() {
                self.stream.write_all(&work.out).await?;
                self.stream.flush().await?;
            }
        }
    }

    /// The startup exchange. Returns false when the connection should simply close.
    async fn startup(&mut self) -> std::io::Result<bool> {
        loop {
            let Some(packet) = self.read_startup_packet().await? else {
                return Ok(false);
            };
            let startup = match decode_startup(&packet) {
                Ok(startup) => startup,
                Err(error) => {
                    self.send_error(&error).await?;
                    return Ok(false);
                }
            };
            match startup {
                // A single `N` is the documented refusal, and the client then either continues in
                // the clear or gives up — either way this is not an error, which is why the loop
                // goes round rather than returning.
                //
                // **The refusal is logged, because the client's half of it is invisible here.** A
                // client on `sslmode=require` reads the `N` and closes without sending a startup
                // packet, so what an operator sees from this end is a connection that opened and
                // went away: no user, no database, no error. The line below is the only place that
                // says why, and it names the way out. GSSAPI encryption is refused the same way
                // and stays refused — nothing in this project speaks it (ADR 0055).
                Startup::SslRequest | Startup::GssEncRequest => {
                    refuse(&mut self.stream, &self.config.tls).await?;
                }
                // **The protocol gives no reply to a `CancelRequest`, even when it works**, so
                // this asks and closes either way — a wrong pid and a wrong key are indistinguishable
                // from a right one from the client's side, which is what leaves nothing to guess
                // against.
                Startup::Cancel { pid, key } => {
                    crate::session::cancel(pid, key);
                    return Ok(false);
                }
                Startup::Parameters { .. } => {
                    return self.complete_startup(&startup).await;
                }
            }
        }
    }

    async fn complete_startup(&mut self, startup: &Startup) -> std::io::Result<bool> {
        if let Startup::Parameters { parameters, .. } = startup {
            if let Some((_, user)) = parameters.iter().find(|(name, _)| name == "user") {
                self.user.clone_from(user);
            }
            // **`database` defaults to `user`**, which is PostgreSQL's own rule: `psql` with no
            // `-d` sends no `database` parameter at all and lands in the database named for the
            // role. An empty user leaves it empty, and the directory answers `3D000` for that.
            self.database = parameters
                .iter()
                .find(|(name, _)| name == "database")
                .map_or_else(|| self.user.clone(), |(_, value)| value.clone());
            // libpq's `options`, which is a command line and is applied once the session exists
            // (`Connection::apply_options`).
            if let Some((_, options)) = parameters.iter().find(|(name, _)| name == "options") {
                self.options.clone_from(options);
            }
            // **Kept for the view and not applied to anything**, which is what it is on a real
            // server too: `application_name` is a `GUC` a client sets to label itself, and every
            // reader of it is a report. `psql` sends `psql`; a client that sends none shows as
            // the empty string, measured (`debts-v1.1.md` #47).
            if let Some((_, name)) = parameters
                .iter()
                .find(|(name, _)| name == "application_name")
            {
                self.application_name.clone_from(name);
            }
        }
        self.out.clear();
        match negotiation(startup) {
            Negotiation::Proceed => {}
            Negotiation::Downgrade {
                unsupported_options,
            } => {
                // Before anything else, and then the ordinary sequence continues. A refusal here
                // would make this server unreachable by a client that asked for 3.2.
                Message::NegotiateProtocolVersion {
                    newest_minor: PROTOCOL_MINOR,
                    unsupported_options: &unsupported_options,
                }
                .encode(&mut self.out);
            }
            Negotiation::Unsupported { major } => {
                let error = SqlError::ProtocolViolation(format!(
                    "unsupported frontend protocol {major}.0: server supports 3.0"
                ));
                self.send_error(&error).await?;
                return Ok(false);
            }
        }

        if !self.authenticate().await? {
            return Ok(false);
        }

        // Settings a client is entitled to know without asking. `client_encoding` and
        // `standard_conforming_strings` in particular change how a driver escapes what it sends.
        for (name, value) in [
            ("server_version", self.config.server_version.as_str()),
            ("server_encoding", "UTF8"),
            ("client_encoding", "UTF8"),
            ("DateStyle", "ISO, MDY"),
            ("TimeZone", "UTC"),
            ("integer_datetimes", "on"),
            ("standard_conforming_strings", "on"),
        ] {
            Message::ParameterStatus { name, value }.encode(&mut self.out);
        }
        // **A real pid and a real secret.** They were `0, 0` while nothing could cancel: a key
        // that is a constant is worse than no key, because it looks like one. The pair is what a
        // `CancelRequest` on another connection must present to stop this session's statement.
        let backend = crate::session::register();
        self.backend = Some(backend.clone());
        Message::BackendKeyData {
            pid: backend.pid,
            key: backend.key,
        }
        .encode(&mut self.out);
        // **`ReadyForQuery` is not sent here**, and that is the whole of `test_bad_connection`.
        // It is the message that tells a client its connection is established: libpq's `PQconnect`
        // returns when it arrives, and an error after it is an error on a *live* connection —
        // which the client meets later, as `PQconsumeInput() FATAL: …`, from wherever it happens
        // to be reading. `ActiveRecord`'s `new_client` rescues `PG::Error` **from the connect**
        // and reads the database name out of the message; an error that arrives afterwards never
        // reaches it and becomes a `ConnectionNotEstablished` from somewhere else entirely.
        //
        // So readiness waits until the session exists, which is where the database this packet
        // named is resolved (`Connection::run`). A real server validates it earlier still, before
        // authentication; here it needs the executor, and anywhere before `ReadyForQuery` is
        // early enough for every client.
        self.stream.write_all(&self.out).await?;
        self.stream.flush().await?;
        Ok(true)
    }

    /// The session this connection runs as, or `None` when the client has been told why not.
    ///
    /// **After the startup packet, because the database it names is what decides the tenant.** A
    /// name the directory does not have is `3D000` here and the connection ends, which is what
    /// tells `rake db:create` it has work to do — and it is answered **before readiness**, so it
    /// is a connection that never opened rather than one that opened and died. `ActiveRecord`'s
    /// `NoDatabaseError` and `rake db:create` both look at the connect.
    ///
    /// **Onto a blocking thread, and this is the second time this class of bug has been found in
    /// this file.** `for_session` looks like bookkeeping and is not: against a real cluster it
    /// begins a transaction and reads the catalog, which goes `StoreTxn` -> `Router` ->
    /// `TcpStores` -> `BlockingTransport::call` -> `Runtime::block_on`, and building a runtime
    /// inside `#[tokio::main]`'s panics with "Cannot start a runtime from within a runtime". Every
    /// connection completed its startup burst and then died, on every real cluster, from 0510b44e
    /// (the startup packet selects the database) onwards — v1.0.0 included.
    ///
    /// The statement path has been on the blocking pool since it was written, with a comment
    /// saying why; session *creation* was not, and no test started a real node from a shell until
    /// the mpp lane did.
    async fn open_session(
        &mut self,
        executors: Arc<dyn Executors>,
    ) -> std::io::Result<Option<Box<dyn Execute + Send>>> {
        // The identity announced at startup, not a fresh one: `BackendKeyData` already told the
        // client this pid and key.
        let identity = self
            .backend
            .clone()
            .unwrap_or_else(crate::session::register);
        // **Who this session is, written once and never again.** The startup packet is gone by
        // the time a statement runs, and `pg_stat_activity` reported NULL for every one of these
        // — which is how r1 came to have three thousand sessions it could count and not chase
        // (`debts-v1.1.md` #47).
        if let Ok(mut activity) = identity.activity.lock() {
            activity.client = crate::session::Client {
                user: self.user.clone(),
                application_name: self.application_name.clone(),
                address: self.peer.map(|peer| peer.ip().to_string()),
                port: self.peer.map(|peer| i32::from(peer.port())),
                started: Some(wall_clock_micros()),
            };
        }
        let database = self.database.clone();
        let made = tokio::task::spawn_blocking(move || executors.for_session(&database, identity))
            .await
            .map_err(std::io::Error::other)?;
        match made {
            Ok(mut executor) => {
                // **The startup packet's `options`, applied before the client is told it may
                // speak.** A parameter the server cannot honour fails the connection on a real
                // server rather than being dropped, and a dropped one leaves the client believing
                // a setting it does not have — `connection_test.rb` connects with `-c geqo=off`
                // and then asks `SHOW geqo`.
                if let Err(error) = self.apply_options(executor.as_mut()) {
                    self.send_error(&error).await?;
                    return Ok(None);
                }
                self.announce_ready().await?;
                Ok(Some(executor))
            }
            Err(error) => {
                self.send_error(&error).await?;
                Ok(None)
            }
        }
    }

    /// Applies every parameter the startup packet's `options` asked for.
    fn apply_options(&self, executor: &mut dyn Execute) -> Result<()> {
        for (name, value) in crate::parameter::command_line(&self.options)? {
            executor.set_option(&name, &value)?;
        }
        Ok(())
    }

    /// Tells the client its connection is established.
    ///
    /// Sent once the session exists, never before: see the note at the end of `complete_startup`.
    async fn announce_ready(&mut self) -> std::io::Result<()> {
        self.out.clear();
        Message::ReadyForQuery(TransactionStatus::Idle).encode(&mut self.out);
        self.stream.write_all(&self.out).await?;
        self.stream.flush().await
    }

    /// Returns false when authentication failed and the connection should close.
    async fn authenticate(&mut self) -> std::io::Result<bool> {
        match self.config.auth {
            Auth::Trust => {
                Message::AuthenticationOk.encode(&mut self.out);
                Ok(true)
            }
            Auth::Cleartext { expected } => {
                Message::AuthenticationCleartextPassword.encode(&mut self.out);
                self.stream.write_all(&self.out).await?;
                self.stream.flush().await?;
                self.out.clear();

                let Some((tag, body)) = self.read_message().await? else {
                    return Ok(false);
                };
                // The password arrives NUL-terminated; comparing without stripping it would refuse
                // every correct password.
                let supplied = if tag == b'p' {
                    body.split(|byte| *byte == 0).next().unwrap_or_default()
                } else {
                    return Ok(false);
                };
                if supplied == expected.as_bytes() {
                    Message::AuthenticationOk.encode(&mut self.out);
                    Ok(true)
                } else {
                    let error = SqlError::InvalidPassword(self.user.clone());
                    self.send_error(&error).await?;
                    Ok(false)
                }
            }
        }
    }

    /// Reads a startup packet, or hands back the one the accept path already read.
    ///
    /// The accept path has to read the first packet itself to answer `SSLRequest` before there is
    /// a session at all; when that packet turns out to be an ordinary startup message it is
    /// carried here rather than pushed back onto the socket. There is still only one reader
    /// ([`read_startup_packet`]) and one decoder, which is the point.
    async fn read_startup_packet(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        if let Some(packet) = self.pending.take() {
            return Ok(Some(packet));
        }
        read_startup_packet(&mut self.stream).await
    }

    /// Reads one framed message: a tag byte, a length that counts itself but not the tag, a body.
    async fn read_message(&mut self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut header = [0u8; 5];
        if !self.read_exact_or_eof(&mut header).await? {
            return Ok(None);
        }
        let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        // The length includes its own four bytes, so anything below that is malformed, and the cap
        // is what stops a client reserving a gigabyte by claiming one.
        if !(4..=MAX_MESSAGE_LEN).contains(&length) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message length {length} is out of range"),
            ));
        }
        let mut body = vec![0u8; length - 4];
        if !self.read_exact_or_eof(&mut body).await? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection ended in the middle of a message",
            ));
        }
        Ok(Some((header[0], body)))
    }

    /// Reads exactly `buffer.len()` bytes, or reports a clean end of stream.
    ///
    /// A client that hangs up between messages is ordinary, not an error; one that hangs up
    /// *inside* a message is a real failure and `read_exact` reports it.
    ///
    /// **[`Connection::spare`] comes first**, and every read goes through here for that reason:
    /// bytes the watcher took off the socket while a statement ran are part of the same stream and
    /// have to be framed in the order the client wrote them.
    async fn read_exact_or_eof(&mut self, buffer: &mut [u8]) -> std::io::Result<bool> {
        let taken = self.spare.len().min(buffer.len());
        buffer[..taken].copy_from_slice(&self.spare[..taken]);
        self.spare.drain(..taken);
        if taken == buffer.len() {
            return Ok(true);
        }
        match read_exact_or_eof(&mut self.stream, &mut buffer[taken..]).await? {
            true => Ok(true),
            // Nothing was in hand, so this is the ordinary hang-up between messages.
            false if taken == 0 => Ok(false),
            // Bytes were in hand and the rest never came: the same half-message failure
            // `read_exact_or_eof` reports, one layer up.
            false => Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the connection ended in the middle of a message",
            )),
        }
    }

    /// Runs one message's work on the blocking pool, **watching the socket while it runs**.
    ///
    /// The bundle moves in and comes back out, so the reply buffer really is reused across
    /// messages rather than reallocated per statement, and `spawn_blocking` rather than
    /// `block_in_place` because a node serves many connections at once: blocking a *worker* thread
    /// per statement would starve the runtime of the threads it needs to read the next message,
    /// where the blocking pool exists to be blocked.
    ///
    /// The watching half is `debts-v1.1.md` #47 and is described on
    /// [`Connection::watch_for_the_client_leaving`]. Nothing here closes anything: the watcher
    /// sets the cancellation flag, the statement ends with `57014`, and the read at the top of the
    /// loop finds the end of the stream and ends the session the ordinary way.
    async fn handle_while_watching(
        &mut self,
        work: Work,
        message: crate::pgwire::message::Frontend,
    ) -> std::io::Result<Work> {
        let leaving = self
            .backend
            .as_ref()
            .map(|backend| Arc::clone(&backend.cancel));
        let mut running = tokio::task::spawn_blocking(move || {
            let mut work = work;
            // **This message's reply, and nothing before it.** The buffer is reused across
            // messages and arrives holding the *startup* reply on the first trip round, because
            // `run` takes it from the connection after startup has built its answers in it — so
            // leaving this out sends the startup reply again in front of the first statement's,
            // and a client that reads to `ReadyForQuery` stops at the one it has already seen:
            // `Answer { tags: "Z", rows: [] }` for `SELECT 1`. It was a line in the loop before
            // this function was extracted, and it belongs to the message rather than to the loop.
            work.out.clear();
            work.session
                .handle(&message, work.executor.as_mut(), &mut work.out);
            work
        });
        // The borrows of `self` end with this block, so the join below is free to take `running`
        // on the path where the watcher won the race.
        let finished = {
            let watching = Self::watch_for_the_client_leaving(
                &mut self.stream,
                &mut self.spare,
                leaving.as_ref(),
            );
            tokio::pin!(watching);
            tokio::select! {
                done = &mut running => done,
                // The watcher never finishes — it has nothing to hand back and the statement is
                // the only thing that ends the race, so it says so in its type.
                never = &mut watching => match never {},
            }
        };
        finished.map_err(std::io::Error::other)
    }

    /// Watches the socket **while a statement runs**, and cancels the statement when the client
    /// has gone.
    ///
    /// Returns when there is nothing more to watch for: the peer closed, the socket failed, or so
    /// much was pipelined that holding it is worse than not watching.
    ///
    /// **A socket only says the peer is gone by returning zero from a read**, which is why this
    /// exists at all: between statements the connection loop is already blocked in a read and
    /// notices at once — a client killed while idle is reaped in about eight seconds, measured —
    /// but *during* a statement nobody is reading, so the `FIN` sits in the kernel and the session
    /// and its `CLOSE_WAIT` socket live until the statement ends on its own. `SELECT pg_sleep(60)`
    /// killed immediately still held both for the full sixty seconds; a statement that never ends
    /// held them for ever, which is how run 112 accumulated about three thousand sessions that
    /// `pg_stat_activity` never gave back (`debts-v1.1.md` #47, r1's
    /// `results/run-112b/session-leak.md`).
    ///
    /// **It sets the cancellation flag rather than closing anything.** That flag is the one
    /// `pg_cancel_backend` and the protocol's `CancelRequest` already set, and `exec::cancel`
    /// already checks it between units of work — the scan walk, the row wait and `pg_sleep` — so
    /// the wait this row was found on is interruptible without a second mechanism. The statement
    /// ends with `57014`, the loop goes round, and the read that follows finds the end of the
    /// stream and closes the session properly, unwinding whatever it held.
    async fn watch_for_the_client_leaving(
        stream: &mut S,
        spare: &mut Vec<u8>,
        cancel: Option<&Arc<std::sync::atomic::AtomicBool>>,
    ) -> std::convert::Infallible {
        let mut buffer = [0u8; 4096];
        loop {
            // **A cap, because a watcher is not a queue.** A client may pipeline behind its own
            // statement and this has to keep those bytes; a client that floods must not be able to
            // make the server hold them. Past the cap the watch simply stops: the connection is
            // then exactly as it was before this function existed, which is a leak and never a
            // corruption.
            if spare.len() > MAX_MESSAGE_LEN {
                break;
            }
            match stream.read(&mut buffer).await {
                // Zero is the peer's `FIN`; an error is an `RST` or worse. Both mean nobody is
                // waiting for this statement's answer.
                //
                // **It goes on insisting rather than setting the flag once**, and that is a race
                // this cannot otherwise win: `exec::cancel::with_session` **clears** the flag at
                // the start of every statement — deliberately, so a `CancelRequest` that arrives
                // while a session is idle cannot kill the *next* statement — and it does that on
                // the blocking thread, after this task has already started watching. A client
                // that was gone before its statement got going would have its one store wiped by
                // that clear, and the statement would run to completion: harmless for a `SELECT`,
                // and not harmless at all for the case this row was found on, where the statement
                // is blocked on a lock held by another dead session and would never end.
                //
                // So it stores in a loop, and the loop lives exactly as long as the statement:
                // `handle_while_watching` drops this future the moment the work completes. Fifty
                // milliseconds is far below any wait worth cancelling and far above the window the
                // clear can hide in.
                Ok(0) | Err(_) => loop {
                    if let Some(cancel) = cancel {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                },
                Ok(read) => spare.extend_from_slice(&buffer[..read]),
            }
        }
        // Nothing left to watch for, and nothing to report: the statement is what this races, and
        // it is the only thing that ends the race. Returning would make the caller join a future
        // it has not been told anything by.
        std::future::pending().await
    }

    /// Sends one error and flushes. Used on the paths where the connection is about to end, which
    /// is why it does not go through the session.
    async fn send_error(&mut self, error: &SqlError) -> std::io::Result<()> {
        let mut fields = error_fields(error);
        // An error that ends the connection is FATAL to the client even when the condition itself
        // is ordinary; a client told `ERROR` would wait for a `ReadyForQuery` that is not coming.
        if error.severity() != Severity::Fatal {
            for (_, value) in fields.iter_mut().take(2) {
                value.clear();
                value.push_str("FATAL");
            }
        }
        let mut bytes = Vec::new();
        error_message(error, &fields).encode(&mut bytes);
        self.stream.write_all(&bytes).await?;
        self.stream.flush().await
    }
}

/// Convenience for the common case: one backend, and an executor per session built from it.
///
/// **Nothing in this workspace constructs one.** Its doc said "the executor itself lands in unit 6;
/// until then this exists so the listener can be stood up and pointed at something", and unit 6
/// landed long ago — so this is scaffolding whose stated deadline has passed and is a deletion
/// candidate. Kept and updated here rather than removed, because removing a `pub` item is not what
/// this change was asked to do.
#[derive(Debug)]
pub struct SharedBackend<F> {
    /// Builds a session executor from the identity that session was announced under.
    pub make: F,
}

impl<F> Executors for SharedBackend<F>
where
    F: Fn(crate::session::Backend) -> Box<dyn Execute + Send> + Send + Sync + 'static,
{
    fn for_session(
        &self,
        _database: &str,
        identity: crate::session::Backend,
    ) -> Result<Box<dyn Execute + Send>> {
        Ok((self.make)(identity))
    }
}

/// Negotiates encryption, then runs the connection to completion.
///
/// The one place both listeners meet, so `SSLRequest` is answered identically whether the node was
/// started by [`serve`] or by a test through [`serve_on`].
async fn accept<S>(
    stream: S,
    peer: std::net::SocketAddr,
    config: Config,
    executors: Arc<dyn Executors>,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let tls = config.tls.clone();
    let (stream, pending) = negotiate(stream, &tls).await?;
    Connection::resuming(stream, config, pending)
        .from_peer(peer)
        .run(executors)
        .await
}

/// The wall clock, in microseconds since the PostgreSQL epoch.
///
/// **The only reading of the wall clock in this crate**, and it is for `backend_start` alone —
/// see [`crate::session::Client::started`] for why a connection cannot use the clock invariant 6
/// names, and for the ruling that a display timestamp may use this one.
fn wall_clock_micros() -> i64 {
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_micros()).unwrap_or(0));
    since_unix - crate::value::timestamp::PG_EPOCH_UNIX_SECONDS * 1_000_000
}

/// Reads a startup packet: a four-byte length that counts itself, then the rest.
///
/// Free rather than a method because the accept path reads one before a [`Connection`] exists —
/// the `SSLRequest` that decides whether the rest of the conversation is encrypted arrives before
/// there is a session to own it.
async fn read_startup_packet<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut length_bytes = [0u8; 4];
    if !read_exact_or_eof(stream, &mut length_bytes).await? {
        return Ok(None);
    }
    let length = u32::from_be_bytes(length_bytes) as usize;
    if !(8..=MAX_MESSAGE_LEN).contains(&length) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("startup packet length {length} is out of range"),
        ));
    }
    let mut packet = length_bytes.to_vec();
    packet.resize(length, 0);
    stream.read_exact(&mut packet[4..]).await?;
    Ok(Some(packet))
}

/// Reads exactly `buffer.len()` bytes, or reports a clean end of stream.
async fn read_exact_or_eof<S: AsyncRead + Unpin>(
    stream: &mut S,
    buffer: &mut [u8],
) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buffer.len() {
        let read = stream.read(&mut buffer[filled..]).await?;
        if read == 0 {
            return if filled == 0 {
                Ok(false)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the connection ended in the middle of a message",
                ))
            };
        }
        filled += read;
    }
    Ok(true)
}

/// Answers the client's encryption request, if it makes one, and returns the stream to serve on.
///
/// **This runs before any framed message, because the protocol puts it there.** `SSLRequest` is
/// eight bytes in the position a startup packet would occupy, and the answer — one byte, `S` or
/// `N`, with no length and no tag — decides whether everything after it is inside TLS records.
/// The packet that is *not* an encryption request is handed back with the stream rather than
/// pushed back onto the socket, so the framed reader never sees a partial one.
///
/// A client may ask more than once (`psql` with `gssencmode` tries GSSAPI first), so this loops:
/// GSSAPI is always refused, and a second `SSLRequest` after the connection is already encrypted
/// is refused too rather than nesting a session inside itself.
async fn negotiate<S>(
    mut stream: S,
    tls: &TlsConfig,
) -> std::io::Result<(MaybeTlsStream<S>, Option<Vec<u8>>)>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    loop {
        let Some(packet) = read_startup_packet(&mut stream).await? else {
            return Ok((MaybeTlsStream::Plain(stream), None));
        };
        // A packet this cannot decode is not this function's to answer: handing it on lets the
        // session reply with a real `ErrorResponse` the way it always has.
        let Ok(startup) = decode_startup(&packet) else {
            return Ok((MaybeTlsStream::Plain(stream), Some(packet)));
        };
        match startup {
            Startup::SslRequest => {
                #[cfg(feature = "tls")]
                if let Some(server) = tls.server() {
                    stream.write_all(b"S").await?;
                    stream.flush().await?;
                    // From here the client starts a TLS handshake and then sends its real startup
                    // packet inside it, so there is nothing pending: the session reads it through
                    // the encrypted stream like any other.
                    let encrypted =
                        esker_proto::transport::tls::accept(stream, Arc::clone(server)).await?;
                    return Ok((MaybeTlsStream::Tls(encrypted), None));
                }
                refuse(&mut stream, tls).await?;
            }
            Startup::GssEncRequest => refuse(&mut stream, tls).await?,
            Startup::Cancel { .. } | Startup::Parameters { .. } => {
                return Ok((MaybeTlsStream::Plain(stream), Some(packet)));
            }
        }
    }
}

/// Answers `N` to an encryption request, and says why in the log.
///
/// The client's half of this is invisible from here: `libpq` on `sslmode=require` reads the `N` and
/// closes without sending a startup packet, so what an operator sees is a connection that opened
/// and went away — no user, no database, no error. This line is the only place that says why, and
/// it names the two ways out.
async fn refuse<S: AsyncWrite + Unpin>(stream: &mut S, tls: &TlsConfig) -> std::io::Result<()> {
    tracing::debug!(
        tls_available = tls.is_enabled(),
        "refusing an encryption request: put a TLS-terminating proxy in front of this node, or \
         build with `--features tls` and pass --tls-cert/--tls-key"
    );
    stream.write_all(b"N").await?;
    stream.flush().await
}

/// Binds a listener without serving, so a caller can learn the port before clients arrive.
///
/// # Errors
///
/// Fails if the address cannot be bound.
pub async fn bind(address: &str) -> std::io::Result<TcpListener> {
    TcpListener::bind(address).await
}

/// Serves an already-bound listener. Split from [`serve`] so a test can bind port 0, read back the
/// real port, and only then start accepting.
///
/// # Errors
///
/// Fails only if accepting does; a failure on an individual connection is logged.
pub async fn serve_on(
    listener: TcpListener,
    config: Config,
    executors: Arc<dyn Executors>,
) -> std::io::Result<()> {
    loop {
        let (stream, peer) = listener.accept().await?;
        let config = config.clone();
        let executors = Arc::clone(&executors);
        tokio::spawn(async move {
            let _ = stream.set_nodelay(true);
            if let Err(error) = accept(stream, peer, config, Arc::clone(&executors)).await {
                tracing::debug!(%peer, %error, "connection ended");
            }
        });
    }
}

/// Silences the unused-import warning for `Backend` while the executor is still unit 6's work.
const _: Option<&dyn Backend> = None;

/// An executor that runs nothing, for the tests that are about the protocol alone.
///
/// The real one is [`crate::exec::Executor`]. This one answers every statement with `0A000`
/// naming it, which is contract C2 at the socket, and it lets a test drive a whole session --
/// startup, transaction status, extended-protocol failure -- without a store behind it.
#[derive(Debug, Default)]
pub struct NotYetExecuting;

impl Execute for NotYetExecuting {
    /// There is no session yet, so there is nothing to terminate.
    fn terminated(&self) -> bool {
        false
    }

    /// Discarded: there is no catalog behind this, so nothing can read the view they would fill.
    fn remember_prepared(&mut self, _statements: Vec<crate::session::PreparedStatement>) {}

    /// Nothing here has parameters to set.
    fn set_option(&mut self, _name: &str, _value: &str) -> Result<()> {
        Ok(())
    }

    /// There is no planner behind this, so there is no plan to explain.
    fn explain_prepared(
        &mut self,
        _explain: &crate::parse::Parsed,
        statement: &crate::parse::Parsed,
        _params: &crate::pgwire::session::Params<'_>,
    ) -> Result<crate::pgwire::session::Outcome> {
        Err(SqlError::unsupported(format!(
            "{} (the executor lands in unit 6 of docs/plans/phase-6a.md)",
            statement
                .class()
                .unsupported_feature()
                .unwrap_or("EXPLAIN of a prepared statement")
        )))
    }

    fn execute(
        &mut self,
        parsed: &crate::parse::Parsed,
        _params: &crate::pgwire::session::Params<'_>,
    ) -> Result<crate::pgwire::session::Outcome> {
        Err(SqlError::unsupported(format!(
            "{} (the executor lands in unit 6 of docs/plans/phase-6a.md)",
            parsed
                .class()
                .unsupported_feature()
                .unwrap_or("statement execution")
        )))
    }
}
