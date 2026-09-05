//! A PostgreSQL client, small enough to time with.
//!
//! The benchmark drives a real `esker-sql` node over its real port, because a measurement that
//! went in through the library would not measure the node. That needs a client, and this project
//! has none: `esker-sql` speaks the protocol, `psql` is what a human uses, and nothing in the
//! workspace connects. So this is one — the **simple query** half of protocol 3.0 and nothing
//! else, hand-framed like every other format here (`CLAUDE.md`, `docs/DESIGN.md` §9).
//!
//! It is deliberately not a general client. There is no extended protocol, no binary format, no
//! `COPY`, no cancellation and no pipelining: every one of those would change what a statement
//! costs, and a benchmark's client must be the least interesting thing in the measurement.
//!
//! # Why the timing lives here
//!
//! [`Pg::timed`] brackets *one* statement between the write of its `Query` message and the
//! `ReadyForQuery` that ends it. That is the whole of what a client can see, and naming it here —
//! rather than letting a caller wrap any sequence of calls — is what stops a "query time" that
//! quietly included a `SET` or a reconnect.

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Protocol 3.0, the only version this speaks.
const PROTOCOL_VERSION: i32 = 196_608;

/// How long a read waits before the connection is called dead.
///
/// Generous because a measured statement is *meant* to be slow: this bounds a wedged node, not a
/// working one, and a benchmark that timed out at the length of its own workload would report a
/// transport failure as a result.
const IO_TIMEOUT: Duration = Duration::from_secs(600);

/// What a statement answered.
#[derive(Debug, Default)]
pub(crate) struct Answer {
    /// The column names, from `RowDescription`; empty for a statement that returns none.
    pub(crate) columns: Vec<String>,
    /// One entry per `DataRow`, one `Option` per column — `None` is SQL `NULL`.
    pub(crate) rows: Vec<Vec<Option<String>>>,
    /// The `CommandComplete` tag, e.g. `INSERT 0 1000`.
    pub(crate) tag: String,
}

impl Answer {
    /// Every row's first column, joined by newlines — how `EXPLAIN` is read.
    pub(crate) fn text(&self) -> String {
        self.rows
            .iter()
            .filter_map(|row| row.first().cloned().flatten())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The one value of a one-row, one-column answer, or an error naming what came back instead.
    pub(crate) fn scalar(&self) -> Result<&str, String> {
        match self.rows.as_slice() {
            [row] => match row.as_slice() {
                [Some(value)] => Ok(value),
                other => Err(format!("expected one column, got {}", other.len())),
            },
            other => Err(format!("expected one row, got {}", other.len())),
        }
    }
}

/// One connection to a SQL node.
#[derive(Debug)]
pub(crate) struct Pg {
    stream: BufReader<TcpStream>,
    out: Vec<u8>,
}

impl Pg {
    /// Connects, sends the startup packet and reads through to the first `ReadyForQuery`.
    ///
    /// Only `AuthenticationOk` is accepted. A node asking for a password is a node this benchmark
    /// did not start, and guessing at one would be a worse failure than refusing.
    pub(crate) fn connect(address: &str, user: &str, database: &str) -> Result<Self, String> {
        let socket = TcpStream::connect(address)
            .map_err(|error| format!("connecting to {address}: {error}"))?;
        socket
            .set_nodelay(true)
            .map_err(|error| format!("setting TCP_NODELAY on {address}: {error}"))?;
        for (what, applied) in [
            ("read", socket.set_read_timeout(Some(IO_TIMEOUT))),
            ("write", socket.set_write_timeout(Some(IO_TIMEOUT))),
        ] {
            applied.map_err(|error| format!("setting the {what} timeout on {address}: {error}"))?;
        }

        let mut startup = Vec::new();
        startup.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        for (key, value) in [("user", user), ("database", database)] {
            startup.extend_from_slice(key.as_bytes());
            startup.push(0);
            startup.extend_from_slice(value.as_bytes());
            startup.push(0);
        }
        startup.push(0);

        let mut pg = Self {
            stream: BufReader::new(socket),
            out: Vec::with_capacity(1 << 16),
        };
        // The startup packet has no tag byte and a length that counts itself — the one message in
        // the protocol framed differently from all the others.
        let length = i32::try_from(startup.len() + 4)
            .map_err(|_| "the startup packet does not fit in an i32".to_owned())?;
        pg.out.extend_from_slice(&length.to_be_bytes());
        pg.out.extend_from_slice(&startup);
        pg.flush_out()?;
        pg.read_until_ready()?;
        Ok(pg)
    }

    /// Runs one statement and returns what it answered.
    pub(crate) fn query(&mut self, sql: &str) -> Result<Answer, String> {
        self.send_query(sql)?;
        self.read_until_ready()
            .map_err(|why| format!("{sql}: {why}"))
    }

    /// Runs one statement and returns how long the client waited for it, with the answer.
    ///
    /// The clock starts before the `Query` message reaches the socket and stops on
    /// `ReadyForQuery`, so it contains the node's planning, its execution and the round trip —
    /// which is exactly what a user's `\timing` reports and the only decomposition-free number
    /// this benchmark has.
    pub(crate) fn timed(&mut self, sql: &str) -> Result<(Duration, Answer), String> {
        let start = Instant::now();
        self.send_query(sql)?;
        let answer = self
            .read_until_ready()
            .map_err(|why| format!("{sql}: {why}"))?;
        Ok((start.elapsed(), answer))
    }

    /// Runs a statement whose answer is not wanted, failing if it errors.
    pub(crate) fn run(&mut self, sql: &str) -> Result<(), String> {
        self.query(sql).map(|_| ())
    }

    fn send_query(&mut self, sql: &str) -> Result<(), String> {
        self.out.clear();
        self.out.push(b'Q');
        let length = i32::try_from(sql.len() + 5)
            .map_err(|_| "the statement does not fit in an i32".to_owned())?;
        self.out.extend_from_slice(&length.to_be_bytes());
        self.out.extend_from_slice(sql.as_bytes());
        self.out.push(0);
        self.flush_out()
    }

    fn flush_out(&mut self) -> Result<(), String> {
        let socket = self.stream.get_mut();
        socket
            .write_all(&self.out)
            .map_err(|error| format!("writing to the SQL node: {error}"))?;
        socket
            .flush()
            .map_err(|error| format!("flushing to the SQL node: {error}"))?;
        self.out.clear();
        Ok(())
    }

    /// Reads messages until `ReadyForQuery`, collecting rows and the first error.
    ///
    /// An `ErrorResponse` does **not** end the exchange: the server still sends `ReadyForQuery`,
    /// and a client that returned early would leave that byte in the stream to be mistaken for
    /// the *next* statement's answer. So the error is kept and raised once the exchange is over.
    fn read_until_ready(&mut self) -> Result<Answer, String> {
        let mut answer = Answer::default();
        let mut failure: Option<String> = None;
        loop {
            let (tag, body) = self.read_message()?;
            match tag {
                b'Z' => break,
                b'T' => answer.columns = row_description(&body)?,
                b'D' => answer.rows.push(data_row(&body)?),
                b'C' => answer.tag = nul_string(&body, 0)?.0,
                b'E' => {
                    if failure.is_none() {
                        failure = Some(error_response(&body));
                    }
                }
                // AuthenticationOk and every other authentication code: only zero is a node this
                // benchmark can use, and the rest are named rather than ignored.
                b'R' => match body.get(..4).map(|code| i32::from_be_bytes(as_four(code))) {
                    Some(0) => {}
                    Some(code) => {
                        return Err(format!(
                            "the SQL node asked for authentication method {code}; only trust is supported"
                        ));
                    }
                    None => return Err("a truncated Authentication message".to_owned()),
                },
                // ParameterStatus, BackendKeyData, NoticeResponse, EmptyQueryResponse,
                // NoData, ParameterDescription: nothing here reads them.
                b'S' | b'K' | b'N' | b'I' | b'n' | b't' => {}
                other => {
                    return Err(format!(
                        "the SQL node sent message '{}', which this client does not read",
                        char::from(other)
                    ));
                }
            }
        }
        match failure {
            Some(why) => Err(why),
            None => Ok(answer),
        }
    }

    /// One tagged message: a tag byte, a length that counts itself but not the tag, a body.
    fn read_message(&mut self) -> Result<(u8, Vec<u8>), String> {
        let mut header = [0u8; 5];
        self.stream
            .read_exact(&mut header)
            .map_err(|error| format!("reading a message header: {error}"))?;
        let length = i32::from_be_bytes(as_four(&header[1..5]));
        let body_len = usize::try_from(length - 4)
            .map_err(|_| format!("a message claiming length {length}"))?;
        let mut body = vec![0u8; body_len];
        self.stream
            .read_exact(&mut body)
            .map_err(|error| format!("reading a {body_len}-byte message body: {error}"))?;
        Ok((header[0], body))
    }
}

/// The first four bytes of `slice`, which every caller has already bounds-checked.
fn as_four(slice: &[u8]) -> [u8; 4] {
    let mut four = [0u8; 4];
    let take = slice.len().min(4);
    four[..take].copy_from_slice(&slice[..take]);
    four
}

/// The column names of a `RowDescription`.
fn row_description(body: &[u8]) -> Result<Vec<String>, String> {
    let count = usize::from(u16::from_be_bytes([
        *body.first().ok_or("a truncated RowDescription")?,
        *body.get(1).ok_or("a truncated RowDescription")?,
    ]));
    let mut at = 2;
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        let (name, next) = nul_string(body, at)?;
        names.push(name);
        // The eighteen bytes after the name are the table oid, attribute number, type oid, type
        // length, type modifier and format code. Nothing here reads them.
        at = next + 18;
    }
    Ok(names)
}

/// One `DataRow`, as text — the only format this client asks for.
fn data_row(body: &[u8]) -> Result<Vec<Option<String>>, String> {
    let count = usize::from(u16::from_be_bytes([
        *body.first().ok_or("a truncated DataRow")?,
        *body.get(1).ok_or("a truncated DataRow")?,
    ]));
    let mut at = 2;
    let mut row = Vec::with_capacity(count);
    for _ in 0..count {
        let length = i32::from_be_bytes(as_four(
            body.get(at..at + 4).ok_or("a truncated DataRow length")?,
        ));
        at += 4;
        if length < 0 {
            row.push(None);
            continue;
        }
        let width = usize::try_from(length).map_err(|_| "a negative DataRow length".to_owned())?;
        let bytes = body
            .get(at..at + width)
            .ok_or("a DataRow value past the end of the message")?;
        row.push(Some(String::from_utf8_lossy(bytes).into_owned()));
        at += width;
    }
    Ok(row)
}

/// An `ErrorResponse` rendered the way `psql` renders one: severity, code and message.
fn error_response(body: &[u8]) -> String {
    let mut severity = String::new();
    let mut code = String::new();
    let mut message = String::new();
    let mut at = 0;
    while let Some(&field) = body.get(at) {
        if field == 0 {
            break;
        }
        let Ok((value, next)) = nul_string(body, at + 1) else {
            break;
        };
        match field {
            b'S' => severity = value,
            b'C' => code = value,
            b'M' => message = value,
            _ => {}
        }
        at = next;
    }
    format!("{severity} {code}: {message}")
}

/// The NUL-terminated string at `at`, and the offset just past its terminator.
fn nul_string(body: &[u8], at: usize) -> Result<(String, usize), String> {
    let rest = body.get(at..).ok_or("a string past the end of a message")?;
    let end = rest
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("an unterminated string in a message")?;
    Ok((
        String::from_utf8_lossy(&rest[..end]).into_owned(),
        at + end + 1,
    ))
}

#[cfg(test)]
mod tests {
    use super::{Answer, data_row, error_response, nul_string, row_description};

    #[test]
    fn a_row_description_is_read_as_names() {
        let mut body = vec![0, 2];
        for name in ["id", "total"] {
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(&[0u8; 18]);
        }
        assert_eq!(row_description(&body).unwrap(), vec!["id", "total"]);
    }

    #[test]
    fn a_negative_length_is_null_and_not_an_empty_string() {
        let mut body = vec![0, 2];
        body.extend_from_slice(&(-1i32).to_be_bytes());
        body.extend_from_slice(&0i32.to_be_bytes());
        assert_eq!(
            data_row(&body).unwrap(),
            vec![None, Some(String::new())],
            "NULL and '' must not collapse: a benchmark that read one as the other would \
             compare two engines on a value neither returned"
        );
    }

    #[test]
    fn an_error_response_names_its_code() {
        let mut body = Vec::new();
        for (field, value) in [(b'S', "ERROR"), (b'C', "42601"), (b'M', "syntax error")] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        assert_eq!(error_response(&body), "ERROR 42601: syntax error");
    }

    #[test]
    fn an_unterminated_string_is_an_error_and_not_a_panic() {
        assert!(nul_string(b"no terminator", 0).is_err());
        assert!(nul_string(b"", 4).is_err());
    }

    #[test]
    fn a_scalar_answer_refuses_a_shape_that_is_not_one() {
        let one = Answer {
            columns: vec!["n".to_owned()],
            rows: vec![vec![Some("7".to_owned())]],
            tag: "SELECT 1".to_owned(),
        };
        assert_eq!(one.scalar().unwrap(), "7");
        let two = Answer {
            rows: vec![vec![Some("7".to_owned())], vec![Some("8".to_owned())]],
            ..Answer::default()
        };
        assert!(two.scalar().is_err());
    }
}
