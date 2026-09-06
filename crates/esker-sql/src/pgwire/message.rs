//! PostgreSQL protocol v3 messages: decoding what a client sends, encoding what a server answers.
//!
//! Pure. No sockets, no `tokio`, no allocation the caller cannot see — a `&[u8]` goes in and a
//! value comes out, or a `Vec<u8>` is appended to. That is what lets the whole protocol be tested
//! against captured bytes without a listener, and it is why the async edge in
//! [`super::server`](../server/index.html) is as thin as it is.
//!
//! # Where the bytes in the tests came from
//!
//! Not from reading the specification. `tests/golden/pgwire.hex` holds bytes recorded off a socket
//! between a real `psql` 18.6 and a real PostgreSQL 19beta1, and the tests here assert that this
//! encoder produces those bytes and this decoder reads them. Several details in the code below
//! exist because the capture disagreed with what the specification alone would have suggested.
//!
//! # Invariants
//!
//! * **A decoder never panics** (`CLAUDE.md` invariant 9). Every length in a frontend message is
//!   attacker-controlled: a `Bind` can claim eighty thousand parameters in a twelve-byte body, and
//!   the only correct answer is a typed error. Nothing here indexes a slice without checking it.
//! * **A length prefix is the message's own, and it counts itself.** The four length bytes are
//!   included in the count and the one-byte type tag is not, which is the single most common way
//!   to be one byte wrong in this protocol.
//! * **Text is not assumed to be UTF-8 until it is checked.** A client may send anything.

use crate::error::{Result, SqlError};
use crate::value::PgType;

/// The `SSLRequest` code, in place of a protocol version: 1234 in the high 16 bits, 5679 in the low.
pub const SSL_REQUEST_CODE: u32 = 80_877_103;

/// The `GSSENCRequest` code, the same trick one number along.
pub const GSSENC_REQUEST_CODE: u32 = 80_877_104;

/// The `CancelRequest` code.
pub const CANCEL_REQUEST_CODE: u32 = 80_877_102;

/// The protocol this server speaks. A client asking for a later minor version is told this one and
/// carries on ([`Backend::NegotiateProtocolVersion`]).
pub const PROTOCOL_MAJOR: u16 = 3;
/// The minor version this server speaks.
pub const PROTOCOL_MINOR: u16 = 0;

/// Largest frontend message accepted, in bytes.
///
/// PostgreSQL's own limit is a gigabyte; this is smaller because nothing Esker accepts is large —
/// the biggest legitimate message is a `Query` or a `Bind` carrying parameter values. The point of
/// the cap is that the length prefix arrives before the body, so a claim of 900 MB would otherwise
/// have us allocate 900 MB on behalf of anyone who can open a socket.
pub const MAX_MESSAGE_LEN: usize = 64 * 1024 * 1024;

/// What a client sends before the protocol proper begins.
///
/// The startup packet has no type byte — it is the only message that does not — so it is decoded
/// by a different function from everything after it, and the four-byte code where a length would
/// otherwise be tells the four cases apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Startup {
    /// "May I use TLS?" Answered with a single byte, `N` here, since Esker terminates no TLS.
    SslRequest,
    /// "May I use GSSAPI encryption?" Refused the same way.
    GssEncRequest,
    /// A second connection asking to cancel work on the first.
    Cancel {
        /// Process id the server handed out in `BackendKeyData`.
        pid: u32,
        /// Secret from the same message.
        key: u32,
    },
    /// The real thing.
    Parameters {
        /// Protocol major version. Anything but 3 is refused.
        major: u16,
        /// Protocol minor version. A later one is negotiated down, never refused.
        minor: u16,
        /// `user`, `database`, `application_name`, and whatever else the client sent.
        parameters: Vec<(String, String)>,
    },
}

/// A message from the client, after startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frontend {
    /// Simple query protocol: one string, which may hold several statements.
    Query(String),
    /// Extended protocol: prepare a statement.
    Parse {
        /// Prepared statement name; empty is the unnamed statement.
        statement: String,
        /// The SQL.
        sql: String,
        /// Type OIDs the client wants for its parameters; 0 means "you decide".
        param_types: Vec<u32>,
    },
    /// Extended protocol: bind parameter values into a portal.
    Bind {
        /// Portal to create; empty is the unnamed portal.
        portal: String,
        /// Prepared statement to bind.
        statement: String,
        /// One format code per parameter, or one for all, or none meaning all text.
        param_formats: Vec<i16>,
        /// Parameter values. `None` is SQL NULL, which the wire spells as length -1.
        params: Vec<Option<Vec<u8>>>,
        /// Format codes for the result columns, same rule as `param_formats`.
        result_formats: Vec<i16>,
    },
    /// Extended protocol: ask what a statement or portal looks like.
    Describe {
        /// Whether `name` is a statement or a portal.
        target: Target,
        /// The name; empty is the unnamed one.
        name: String,
    },
    /// Extended protocol: run a portal.
    Execute {
        /// The portal to run.
        portal: String,
        /// Row limit, 0 meaning no limit.
        max_rows: u32,
    },
    /// Extended protocol: destroy a statement or portal.
    Close {
        /// Whether `name` is a statement or a portal.
        target: Target,
        /// The name.
        name: String,
    },
    /// End of an extended-protocol batch: finish the transaction step and report readiness.
    Sync,
    /// Push out anything buffered without ending the batch.
    Flush,
    /// The client is leaving.
    Terminate,
    /// A password, in answer to an authentication request.
    Password(Vec<u8>),
    /// A message this server does not implement, kept so the session can refuse it precisely
    /// rather than dropping the connection.
    Unknown {
        /// The type byte.
        tag: u8,
        /// The body, without the tag or the length.
        body: Vec<u8>,
    },
}

/// Whether an extended-protocol message names a prepared statement or a portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// A prepared statement — `S` on the wire.
    Statement,
    /// A portal — `P` on the wire.
    Portal,
}

impl Target {
    /// The wire byte.
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            Target::Statement => b'S',
            Target::Portal => b'P',
        }
    }

    fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            b'S' => Ok(Target::Statement),
            b'P' => Ok(Target::Portal),
            other => Err(SqlError::ProtocolViolation(format!(
                "invalid Describe/Close target {:?}",
                other as char
            ))),
        }
    }
}

/// What the session reports in `ReadyForQuery`, which is how a client knows whether it is inside a
/// transaction and whether that transaction is still usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionStatus {
    /// Not in a transaction block — `I`. A session starts here.
    #[default]
    Idle,
    /// In a transaction block — `T`.
    InTransaction,
    /// In a transaction block that has failed; everything is refused until it ends — `E`.
    Failed,
}

impl TransactionStatus {
    /// The wire byte.
    #[must_use]
    pub fn as_byte(self) -> u8 {
        match self {
            TransactionStatus::Idle => b'I',
            TransactionStatus::InTransaction => b'T',
            TransactionStatus::Failed => b'E',
        }
    }
}

/// One column of a `RowDescription`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldDescription {
    /// Column name as the client should display it.
    pub name: String,
    /// OID of the table the column came from, or 0 when it is not a plain column reference.
    pub table_oid: u32,
    /// Attribute number within that table, or 0.
    pub column_id: i16,
    /// Type OID.
    pub type_oid: u32,
    /// Type length in bytes, negative for a variable-length type.
    pub type_size: i16,
    /// Type modifier, -1 when there is none.
    pub type_modifier: i32,
    /// 0 for text, 1 for binary.
    pub format: i16,
}

impl FieldDescription {
    /// A column of type `ty`, described as a computed value.
    ///
    /// `table_oid` and `column_id` are zero, which the protocol defines as "not a plain column
    /// reference". PostgreSQL fills them in for a column that came straight out of a table; ours
    /// are `u64` relation ids and would not fit the `u32` field, and a truncated one could name a
    /// different relation. Zero is the honest answer and the one the protocol provides for it.
    ///
    /// The type modifier is `-1`: a computed value has no declared length. That is PostgreSQL's
    /// answer too — `c || '|'` over a `character(3)` is `text` with no modifier, and `min(c)` is
    /// `bpchar` with none. Only a **plain column reference** carries one, which is
    /// [`FieldDescription::of`].
    #[must_use]
    pub fn computed(name: impl Into<String>, ty: crate::value::ColumnType) -> Self {
        Self::of(name, ty, crate::value::NO_TYPMOD)
    }

    /// A column of type `ty` carrying the declared length or precision its column was given.
    ///
    /// `typmod` is PostgreSQL's `atttypmod` and travels to the client unchanged, which is what
    /// makes `\gdesc` say `character varying(5)` rather than `character varying`.
    #[must_use]
    pub fn of(name: impl Into<String>, ty: crate::value::ColumnType, typmod: i32) -> Self {
        FieldDescription {
            name: name.into(),
            table_oid: 0,
            column_id: 0,
            type_oid: ty.oid(),
            type_size: ty.type_len(),
            type_modifier: typmod,
            format: 0,
        }
    }

    /// The same, for a column declared as a **user-defined type**.
    ///
    /// The oid is the type's own and the size is the **rendered** value's, not the stored one's:
    /// an enum is an `int2` in the row and a variable-length label on the wire, so the length is
    /// `-1` exactly as it is for `text`. A client reads the oid, asks the catalog what it is, and
    /// gets `typtype = 'e'` — which is how `ActiveRecord` decides a column is an enum (ADR 0050).
    #[must_use]
    pub fn of_user_type(name: impl Into<String>, oid: u32, type_size: i16) -> Self {
        FieldDescription {
            name: name.into(),
            table_oid: 0,
            column_id: 0,
            type_oid: oid,
            type_size,
            type_modifier: crate::value::NO_TYPMOD,
            format: 0,
        }
    }
}

/// One field of an `ErrorResponse` or `NoticeResponse`.
///
/// The wire carries a one-byte code and a string; the codes are single letters whose meanings are
/// fixed by the protocol, and a client picks out the ones it understands and ignores the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorField(pub u8);

impl ErrorField {
    /// Localised severity — `S`. A client that shows text to a human reads this one.
    pub const SEVERITY: ErrorField = ErrorField(b'S');
    /// Unlocalised severity — `V`. A client that *branches* reads this one.
    pub const SEVERITY_UNLOCALIZED: ErrorField = ErrorField(b'V');
    /// SQLSTATE — `C`.
    pub const CODE: ErrorField = ErrorField(b'C');
    /// Primary message — `M`.
    pub const MESSAGE: ErrorField = ErrorField(b'M');
    /// Optional detail — `D`.
    pub const DETAIL: ErrorField = ErrorField(b'D');
    /// Optional hint — `H`.
    pub const HINT: ErrorField = ErrorField(b'H');
    /// One-based character offset into the query — `P`.
    pub const POSITION: ErrorField = ErrorField(b'P');
}

/// A message from the server.
///
/// Borrowed rather than owned wherever it can be, because these are built per row and per query
/// and the values they carry already live in the executor's buffers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend<'a> {
    /// The client may proceed; no authentication is required.
    AuthenticationOk,
    /// Send a password in the clear. Only ever asked over a trusted link.
    AuthenticationCleartextPassword,
    /// "I do not speak the minor version you asked for; here is the newest I have."
    ///
    /// Sent *before* the ordinary startup sequence, which then continues — it is a downgrade
    /// notice, not a refusal. A 3.0-only server that answers an error instead is unreachable by a
    /// client that asked for 3.2, and modern `libpq` asks for 3.2 on request.
    NegotiateProtocolVersion {
        /// Newest minor version this server supports.
        newest_minor: u16,
        /// Names of `_pq_.` protocol options the client asked for that this server does not know.
        unsupported_options: &'a [String],
    },
    /// A server setting the client should track, such as `client_encoding`.
    ParameterStatus {
        /// Setting name.
        name: &'a str,
        /// Setting value.
        value: &'a str,
    },
    /// The key a second connection needs in order to cancel this one's work.
    BackendKeyData {
        /// Identifies this session.
        pid: u32,
        /// Proves the canceller was told the key by this server.
        key: u32,
    },
    /// The server is ready for the next query, and here is the transaction state.
    ReadyForQuery(TransactionStatus),
    /// The shape of the rows that follow.
    RowDescription(&'a [FieldDescription]),
    /// One row. `None` is SQL NULL, which is a length of -1 and not an empty value.
    DataRow(&'a [Option<Vec<u8>>]),
    /// The statement finished; the string is its command tag.
    CommandComplete(&'a str),
    /// The query string was empty. Takes the place of `CommandComplete`.
    EmptyQueryResponse,
    /// A `Parse` succeeded.
    ParseComplete,
    /// A `Bind` succeeded.
    BindComplete,
    /// A `Close` succeeded.
    CloseComplete,
    /// The described statement or portal returns no rows.
    NoData,
    /// Parameter types of a described statement.
    ParameterDescription(&'a [u32]),
    /// `Execute` stopped at its row limit; the portal is still open.
    PortalSuspended,
    /// Something failed.
    Error(&'a [(ErrorField, String)]),
    /// Something is worth saying, and nothing failed.
    Notice(&'a [(ErrorField, String)]),
}

// --- decoding -------------------------------------------------------------------------------

/// Reads big-endian fields out of a message body without ever running off the end.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Reader { bytes, at: 0 }
    }

    fn short(what: &str) -> SqlError {
        SqlError::ProtocolViolation(format!("message ended in the middle of {what}"))
    }

    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| Self::short(what))?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or_else(|| Self::short(what))?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self, what: &str) -> Result<u8> {
        Ok(self.take(1, what)?[0])
    }

    fn i16(&mut self, what: &str) -> Result<i16> {
        let bytes = self.take(2, what)?;
        Ok(i16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A signed 32-bit field. Signed on purpose: a parameter length of -1 is how the wire spells
    /// SQL NULL, so reading it as unsigned would turn a NULL into a four-gigabyte value.
    fn i32(&mut self, what: &str) -> Result<i32> {
        let bytes = self.take(4, what)?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// A NUL-terminated string. Invalid UTF-8 is a protocol violation, not a lossy conversion:
    /// silently replacing a bad byte would put a character in a table name that the client never
    /// sent.
    fn cstring(&mut self, what: &str) -> Result<String> {
        let rest = self.bytes.get(self.at..).ok_or_else(|| Self::short(what))?;
        let end = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or_else(|| Self::short(what))?;
        let text = std::str::from_utf8(&rest[..end])
            .map_err(|_| SqlError::ProtocolViolation(format!("{what} is not valid UTF-8")))?;
        self.at += end + 1;
        Ok(text.to_owned())
    }

    /// How many bytes are left. Used to bound a count before it is trusted.
    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// Reads a count and refuses one that could not possibly fit in what is left.
    ///
    /// This is the check that matters most in this file. `Bind` carries a parameter count, and
    /// without this a twelve-byte message claiming 2^31 parameters would have us reserve capacity
    /// for two billion of them before reading the first.
    fn count(&mut self, what: &str, bytes_each: usize) -> Result<usize> {
        let count = self.i16(what)?;
        let count = usize::try_from(count)
            .map_err(|_| SqlError::ProtocolViolation(format!("negative {what}")))?;
        if count.saturating_mul(bytes_each) > self.remaining() {
            return Err(SqlError::ProtocolViolation(format!(
                "{what} claims {count} entries, more than the message can hold"
            )));
        }
        Ok(count)
    }
}

/// Decodes the startup packet, which is the only message with no type byte.
///
/// Takes the whole packet including its four length bytes, which is what a reader that framed it
/// on the length prefix will have.
pub fn decode_startup(packet: &[u8]) -> Result<Startup> {
    let mut reader = Reader::new(packet);
    let length = reader.u32("the startup length")? as usize;
    if length != packet.len() {
        return Err(SqlError::ProtocolViolation(format!(
            "startup packet says {length} bytes but {} arrived",
            packet.len()
        )));
    }
    let code = reader.u32("the startup version")?;
    match code {
        SSL_REQUEST_CODE => Ok(Startup::SslRequest),
        GSSENC_REQUEST_CODE => Ok(Startup::GssEncRequest),
        CANCEL_REQUEST_CODE => Ok(Startup::Cancel {
            pid: reader.u32("the cancel pid")?,
            key: reader.u32("the cancel key")?,
        }),
        version => {
            let major = u16::try_from(version >> 16).unwrap_or(u16::MAX);
            let minor = u16::try_from(version & 0xFFFF).unwrap_or(u16::MAX);
            let mut parameters = Vec::new();
            // The list ends with an empty name, which is the trailing NUL of the packet.
            loop {
                let name = reader.cstring("a startup parameter name")?;
                if name.is_empty() {
                    break;
                }
                let value = reader.cstring("a startup parameter value")?;
                parameters.push((name, value));
            }
            Ok(Startup::Parameters {
                major,
                minor,
                parameters,
            })
        }
    }
}

/// Decodes one message that has already been framed: `tag` is its type byte and `body` everything
/// after the four length bytes.
pub fn decode(tag: u8, body: &[u8]) -> Result<Frontend> {
    let mut reader = Reader::new(body);
    match tag {
        b'Q' => Ok(Frontend::Query(reader.cstring("the query string")?)),
        b'P' => {
            let statement = reader.cstring("the statement name")?;
            let sql = reader.cstring("the statement text")?;
            let count = reader.count("the parameter type count", 4)?;
            let mut param_types = Vec::with_capacity(count);
            for _ in 0..count {
                param_types.push(reader.u32("a parameter type")?);
            }
            Ok(Frontend::Parse {
                statement,
                sql,
                param_types,
            })
        }
        b'B' => {
            let portal = reader.cstring("the portal name")?;
            let statement = reader.cstring("the statement name")?;
            let format_count = reader.count("the parameter format count", 2)?;
            let mut param_formats = Vec::with_capacity(format_count);
            for _ in 0..format_count {
                param_formats.push(reader.i16("a parameter format")?);
            }
            // Four bytes of length per parameter is the floor, so that is what bounds the count.
            let param_count = reader.count("the parameter count", 4)?;
            let mut params = Vec::with_capacity(param_count);
            for _ in 0..param_count {
                let length = reader.i32("a parameter length")?;
                if length < 0 {
                    // -1 is NULL. Any other negative is nonsense.
                    if length != -1 {
                        return Err(SqlError::ProtocolViolation(format!(
                            "parameter length {length} is negative and is not -1"
                        )));
                    }
                    params.push(None);
                } else {
                    let length = usize::try_from(length).unwrap_or(0);
                    params.push(Some(reader.take(length, "a parameter value")?.to_vec()));
                }
            }
            let result_count = reader.count("the result format count", 2)?;
            let mut result_formats = Vec::with_capacity(result_count);
            for _ in 0..result_count {
                result_formats.push(reader.i16("a result format")?);
            }
            Ok(Frontend::Bind {
                portal,
                statement,
                param_formats,
                params,
                result_formats,
            })
        }
        b'D' => Ok(Frontend::Describe {
            target: Target::from_byte(reader.u8("the describe target")?)?,
            name: reader.cstring("the described name")?,
        }),
        b'E' => Ok(Frontend::Execute {
            portal: reader.cstring("the portal name")?,
            max_rows: reader.u32("the row limit")?,
        }),
        b'C' => Ok(Frontend::Close {
            target: Target::from_byte(reader.u8("the close target")?)?,
            name: reader.cstring("the closed name")?,
        }),
        b'S' => Ok(Frontend::Sync),
        b'H' => Ok(Frontend::Flush),
        b'X' => Ok(Frontend::Terminate),
        b'p' => Ok(Frontend::Password(body.to_vec())),
        other => Ok(Frontend::Unknown {
            tag: other,
            body: body.to_vec(),
        }),
    }
}

// --- encoding -------------------------------------------------------------------------------

/// Appends a message, filling in its length once the body is known.
///
/// The length counts itself and excludes the tag. Writing a placeholder and patching it is the
/// only way to get that right without computing every body length twice.
fn framed(out: &mut Vec<u8>, tag: u8, body: impl FnOnce(&mut Vec<u8>)) {
    out.push(tag);
    let length_at = out.len();
    out.extend_from_slice(&[0; 4]);
    body(out);
    let length = u32::try_from(out.len() - length_at).unwrap_or(u32::MAX);
    out[length_at..length_at + 4].copy_from_slice(&length.to_be_bytes());
}

/// A NUL-terminated string, with any **interior** NUL removed.
///
/// Every string on this wire is a C string, so a NUL inside one does not mean "a NUL" — it means
/// "the string ended here". A value carrying one ends its field early, the reader takes the next
/// byte as the following field's code, and the frame stops agreeing with its length: libpq answers
/// `message contents do not agree with length in message type "E"` and **drops the connection**,
/// because a stream it cannot parse is a stream it cannot resynchronise.
///
/// That is run 54's bug. A raw engine key was rendered into an error message — memcomparable keys
/// are full of `0x00` — and `transactions_test.rb` died rather than failed. The call site is fixed
/// too, but the guarantee belongs here: a statement that fails is ordinary, and no message this
/// node can construct should be able to cost a client its connection.
///
/// **Removed rather than escaped**, which is what a real server's guarantee amounts to: its strings
/// come from a C API and cannot contain one at all, so there is no escaped form to match. Anything
/// wanting to show bytes has to render them printably before it gets here.
fn cstring(out: &mut Vec<u8>, text: &str) {
    if text.as_bytes().contains(&0) {
        out.extend(text.bytes().filter(|byte| *byte != 0));
    } else {
        out.extend_from_slice(text.as_bytes());
    }
    out.push(0);
}

impl Backend<'_> {
    /// Appends this message's bytes, tag and length included.
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Backend::AuthenticationOk => {
                framed(out, b'R', |o| o.extend_from_slice(&0u32.to_be_bytes()));
            }
            Backend::AuthenticationCleartextPassword => {
                framed(out, b'R', |o| o.extend_from_slice(&3u32.to_be_bytes()));
            }
            Backend::NegotiateProtocolVersion {
                newest_minor,
                unsupported_options,
            } => framed(out, b'v', |o| {
                let version = (u32::from(PROTOCOL_MAJOR) << 16) | u32::from(*newest_minor);
                o.extend_from_slice(&version.to_be_bytes());
                let count = u32::try_from(unsupported_options.len()).unwrap_or(u32::MAX);
                o.extend_from_slice(&count.to_be_bytes());
                for option in *unsupported_options {
                    cstring(o, option);
                }
            }),
            Backend::ParameterStatus { name, value } => framed(out, b'S', |o| {
                cstring(o, name);
                cstring(o, value);
            }),
            Backend::BackendKeyData { pid, key } => framed(out, b'K', |o| {
                o.extend_from_slice(&pid.to_be_bytes());
                o.extend_from_slice(&key.to_be_bytes());
            }),
            Backend::ReadyForQuery(status) => framed(out, b'Z', |o| o.push(status.as_byte())),
            Backend::RowDescription(fields) => framed(out, b'T', |o| {
                let count = i16::try_from(fields.len()).unwrap_or(i16::MAX);
                o.extend_from_slice(&count.to_be_bytes());
                for field in *fields {
                    cstring(o, &field.name);
                    o.extend_from_slice(&field.table_oid.to_be_bytes());
                    o.extend_from_slice(&field.column_id.to_be_bytes());
                    o.extend_from_slice(&field.type_oid.to_be_bytes());
                    o.extend_from_slice(&field.type_size.to_be_bytes());
                    o.extend_from_slice(&field.type_modifier.to_be_bytes());
                    o.extend_from_slice(&field.format.to_be_bytes());
                }
            }),
            Backend::DataRow(values) => framed(out, b'D', |o| {
                let count = i16::try_from(values.len()).unwrap_or(i16::MAX);
                o.extend_from_slice(&count.to_be_bytes());
                for value in *values {
                    match value {
                        // NULL is a length of -1 and no bytes. An empty value is a length of 0 and
                        // no bytes, and the two are different rows.
                        None => o.extend_from_slice(&(-1i32).to_be_bytes()),
                        Some(bytes) => {
                            let length = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
                            o.extend_from_slice(&length.to_be_bytes());
                            o.extend_from_slice(bytes);
                        }
                    }
                }
            }),
            Backend::CommandComplete(tag) => framed(out, b'C', |o| cstring(o, tag)),
            Backend::EmptyQueryResponse => framed(out, b'I', |_| {}),
            Backend::ParseComplete => framed(out, b'1', |_| {}),
            Backend::BindComplete => framed(out, b'2', |_| {}),
            Backend::CloseComplete => framed(out, b'3', |_| {}),
            Backend::NoData => framed(out, b'n', |_| {}),
            Backend::ParameterDescription(types) => framed(out, b't', |o| {
                let count = i16::try_from(types.len()).unwrap_or(i16::MAX);
                o.extend_from_slice(&count.to_be_bytes());
                for oid in *types {
                    o.extend_from_slice(&oid.to_be_bytes());
                }
            }),
            Backend::PortalSuspended => framed(out, b's', |_| {}),
            Backend::Error(fields) => framed(out, b'E', |o| encode_fields(o, fields)),
            Backend::Notice(fields) => framed(out, b'N', |o| encode_fields(o, fields)),
        }
    }

    /// This message's bytes on their own.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode(&mut out);
        out
    }
}

/// The field list of an `ErrorResponse` or `NoticeResponse`, terminated by a zero byte where the
/// next field code would be.
fn encode_fields(out: &mut Vec<u8>, fields: &[(ErrorField, String)]) {
    for (field, value) in fields {
        out.push(field.0);
        cstring(out, value);
    }
    out.push(0);
}

/// How many NUL bytes a slice holds. A named function because `iter().filter().count()` over bytes
/// is what clippy calls counting the naive way, and the intent here is worth a name anyway.
#[cfg(test)]
fn bytecount(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .copied()
        .filter(|byte| *byte == 0)
        .fold(0, |n, _| n + 1)
}

#[cfg(test)]
mod wire_tests {
    use super::{Backend, ErrorField, bytecount};

    /// Walks a frame the way libpq does and answers whether the body agrees with the length.
    ///
    /// This is the whole of what the client checks: read the four-byte length, walk the field list
    /// to its terminator, and require that the terminator lands **exactly** at the end. A value
    /// carrying an interior NUL ends the list early and leaves bytes over, which is the
    /// `message contents do not agree with length in message type "E"` a client answers with — and
    /// a client that says that drops the connection rather than failing a statement.
    fn agrees(frame: &[u8]) -> bool {
        assert_eq!(frame[0], b'E', "an ErrorResponse");
        let length = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        if length + 1 != frame.len() {
            return false;
        }
        let body = &frame[5..];
        let mut at = 0;
        while at < body.len() {
            if body[at] == 0 {
                // The terminator: the list ends here, and nothing may follow it.
                return at + 1 == body.len();
            }
            at += 1; // the field code
            let Some(end) = body[at..].iter().position(|byte| *byte == 0) else {
                return false;
            };
            at += end + 1;
        }
        false
    }

    /// **Run 54's protocol bug.** A message carrying a NUL made the frame unreadable, and the
    /// source was a raw engine key rendered into it: a row key is memcomparable and full of
    /// `0x00`, so `could not serialize access due to concurrent update: key t` was everything
    /// before the first one.
    ///
    /// The fix is in the codec rather than at the one call site, because the property has to hold
    /// for **every** message: a statement that fails is ordinary, and a frame a client cannot
    /// parse takes the connection with it.
    #[test]
    fn a_field_value_with_a_nul_still_makes_a_readable_frame() {
        let ordinary = vec![
            (ErrorField::SEVERITY, "ERROR".to_owned()),
            (ErrorField::CODE, "40001".to_owned()),
            (ErrorField::MESSAGE, "plain".to_owned()),
        ];
        assert!(agrees(&Backend::Error(&ordinary).to_bytes()));

        // The shape run 54 met: a key rendered into the text, NULs and all.
        let key = String::from_utf8_lossy(b"r\x00\x00\x00\x00\x00\x00\x00\x01topics").into_owned();
        let with_nuls = vec![
            (ErrorField::SEVERITY, "ERROR".to_owned()),
            (ErrorField::CODE, "40001".to_owned()),
            (
                ErrorField::MESSAGE,
                format!("could not serialize access due to concurrent update: key {key}"),
            ),
        ];
        let frame = Backend::Error(&with_nuls).to_bytes();
        assert!(
            agrees(&frame),
            "the frame must be readable whatever the message carries: {frame:?}"
        );
        // And a NUL must not have been able to end the list early: every field is still there.
        // **Counted past the header**, because the four-byte length is itself mostly zero bytes —
        // a frame this size begins `00 00 00 4e`, and counting those would be counting the ruler.
        assert_eq!(
            bytecount(&frame[5..]),
            with_nuls.len() + 1,
            "one terminator per value and one for the list, and no others"
        );
    }

    /// A `NoticeResponse` is the same frame under another tag, so it needs the same guarantee —
    /// and notices carry user text too (`DROP TABLE IF EXISTS` names the table).
    #[test]
    fn a_notice_is_held_to_the_same_rule() {
        let fields = vec![(ErrorField::MESSAGE, "a\u{0}b".to_owned())];
        let frame = Backend::Notice(&fields).to_bytes();
        assert_eq!(frame[0], b'N');
        let length = u32::from_be_bytes([frame[1], frame[2], frame[3], frame[4]]) as usize;
        assert_eq!(length + 1, frame.len());
        assert_eq!(
            bytecount(&frame[5..]),
            2,
            "the value's terminator and the list's, and no NUL from inside the text"
        );
    }
}
