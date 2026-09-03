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
use tokio::net::{TcpListener, TcpStream};

use crate::backend::Backend;
use crate::error::{Result, Severity, SqlError};
use crate::pgwire::message::{
    Backend as Message, MAX_MESSAGE_LEN, PROTOCOL_MINOR, Startup, TransactionStatus, decode,
    decode_startup,
};
use crate::pgwire::session::{Execute, Session};
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            address: "127.0.0.1:5432".to_owned(),
            auth: Auth::Trust,
            server_version: "19.0 (Esker)".to_owned(),
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
    fn for_session(&self, database: &str) -> Result<Box<dyn Execute + Send>>;
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
            let mut connection = Connection::new(stream, config);
            if let Err(error) = connection.run(executors.as_ref()).await {
                tracing::debug!(%peer, %error, "connection ended");
            }
        });
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
    /// Reused between messages so a busy session is not allocating a buffer per reply.
    ///
    /// It travels to the blocking thread with the session and comes back, so "reused" survives
    /// that trip — which is the point of moving the whole bundle rather than copying out of it.
    out: Vec<u8>,
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
        Connection {
            stream,
            config,

            user: String::new(),
            database: String::new(),
            out: Vec::with_capacity(8 * 1024),
        }
    }

    /// Runs the connection to completion: startup, then messages until the client leaves.
    ///
    /// # Errors
    ///
    /// Any I/O failure. A *protocol* failure is reported to the client and ends the connection
    /// cleanly rather than surfacing here.
    pub async fn run(&mut self, executors: &dyn Executors) -> std::io::Result<()> {
        if !self.startup().await? {
            return Ok(());
        }
        // **After the startup packet, because the database it names is what decides the tenant.**
        // A name the directory does not have is `3D000` here and the connection ends, which is
        // what tells `rake db:create` it has work to do.
        let executor = match executors.for_session(&self.database) {
            Ok(executor) => executor,
            Err(error) => {
                self.send_error(&error).await?;
                return Ok(());
            }
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
                        let _ = work.executor.rollback();
                        self.send_error(&SqlError::IdleInTransactionTimeout).await?;
                        return Ok(());
                    }
                },
                None => self.read_message().await?,
            };
            let Some((tag, body)) = next else {
                return Ok(());
            };
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
                return Ok(());
            }
            // Onto a blocking thread and back. `spawn_blocking` rather than `block_in_place`
            // because a node serves many connections at once: blocking a *worker* thread per
            // statement would starve the runtime of the threads it needs to read the next
            // message, where the blocking pool exists to be blocked.
            work.out.clear();
            // The bundle moves in and comes back out, so the buffer really is reused across
            // messages rather than reallocated per statement.
            work = tokio::task::spawn_blocking(move || {
                let mut work = work;
                work.session
                    .handle(&message, work.executor.as_mut(), &mut work.out);
                work
            })
            .await
            .map_err(std::io::Error::other)?;
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
                // We terminate no TLS and no GSSAPI. A single `N` is the documented refusal, and
                // the client then either continues in the clear or gives up — either way this is
                // not an error, which is why the loop goes round rather than returning.
                Startup::SslRequest | Startup::GssEncRequest => {
                    self.stream.write_all(b"N").await?;
                    self.stream.flush().await?;
                }
                // Cancellation needs a registry of running queries, which arrives with the
                // executor. Closing is what a server that cannot cancel should do: the protocol
                // gives no reply to a CancelRequest even when it works.
                Startup::Cancel { .. } => return Ok(false),
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
        // No cancellation yet, so the key is a constant rather than a secret pretending to be one.
        Message::BackendKeyData { pid: 0, key: 0 }.encode(&mut self.out);
        Message::ReadyForQuery(TransactionStatus::Idle).encode(&mut self.out);
        self.stream.write_all(&self.out).await?;
        self.stream.flush().await?;
        Ok(true)
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

    /// Reads a startup packet: a four-byte length that counts itself, then the rest.
    async fn read_startup_packet(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let mut length_bytes = [0u8; 4];
        if !self.read_exact_or_eof(&mut length_bytes).await? {
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
        self.stream.read_exact(&mut packet[4..]).await?;
        Ok(Some(packet))
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
        self.stream.read_exact(&mut body).await?;
        Ok(Some((header[0], body)))
    }

    /// Reads exactly `buffer.len()` bytes, or reports a clean end of stream.
    ///
    /// A client that hangs up between messages is ordinary, not an error; one that hangs up
    /// *inside* a message is a real failure and `read_exact` reports it.
    async fn read_exact_or_eof(&mut self, buffer: &mut [u8]) -> std::io::Result<bool> {
        let mut filled = 0;
        while filled < buffer.len() {
            let read = self.stream.read(&mut buffer[filled..]).await?;
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
/// The executor itself lands in unit 6; until then this exists so the listener can be stood up and
/// pointed at something.
#[derive(Debug)]
pub struct SharedBackend<F> {
    /// Builds a session executor. Takes the backend so each session gets its own.
    pub make: F,
}

impl<F> Executors for SharedBackend<F>
where
    F: Fn() -> Box<dyn Execute + Send> + Send + Sync + 'static,
{
    fn for_session(&self, _database: &str) -> Result<Box<dyn Execute + Send>> {
        Ok((self.make)())
    }
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
            let mut connection = Connection::<TcpStream>::new(stream, config);
            if let Err(error) = connection.run(executors.as_ref()).await {
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
