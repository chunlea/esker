//! Just enough HTTP/1.1 to carry four S3 calls.
//!
//! Writing a request is easy — we control every byte. **Parsing a response is the dangerous
//! half**, because those bytes come from a server, a proxy, a load balancer, or something that
//! is not any of those and answered on port 9000 anyway. `CLAUDE.md` invariant 9 applies with
//! full force: every malformed byte is an error value, nothing here panics, nothing here
//! allocates on a length a stranger chose.
//!
//! The discipline is `esker-proto`'s: parse forwards, check every length against what is
//! actually present before trusting it, and refuse anything outside the subset we need rather
//! than guessing what it meant. The subset is:
//!
//! * a status line `HTTP/1.x SSS ...`;
//! * headers, `Name: value`, folded lines rejected rather than joined;
//! * a body delimited by `Content-Length`, by `Transfer-Encoding: chunked`, or by the
//!   connection closing.
//!
//! Everything else — `100 Continue` handling, trailers with meaning, content codings,
//! pipelining — is out.
//!
//! **Keep-alive is in**, since [ADR 0039](../../../docs/adr/0039-a-kept-alive-s3-connection.md):
//! a request says `Connection: keep-alive` and [`Response::may_reuse_connection`] says whether the
//! answer allows it. Pipelining stays out, and that is what makes reuse safe to parse for — a
//! response's bytes are the only bytes in flight when it is read, so no reader here can over-read
//! into the next one.

use std::io::Read;

use crate::error::{Error, Result};

/// The verbs the four calls need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GetObject`, `ListObjectsV2`.
    Get,
    /// `PutObject`.
    Put,
    /// `DeleteObject`.
    Delete,
}

impl Method {
    /// The token as it appears on the request line and in the canonical request.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
        }
    }
}

/// The largest header block we will read before deciding the peer is not an HTTP server.
///
/// S3 responses are a few hundred bytes of headers. 64 KiB is generous by two orders of
/// magnitude and bounds what a hostile endpoint can make us allocate before we give up.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The largest response body we will accumulate.
///
/// A ranged read asks for a block; a full read asks for an SST. Both are far below this, and a
/// `Content-Length` above it is refused before a single byte is reserved — which is the point,
/// since the header is a number a stranger chose.
pub const MAX_BODY_BYTES: usize = crate::MAX_SINGLE_PUT;

/// A request, before it is signed.
///
/// `path` is **unencoded**: `/bucket/prefix/000007.sst`, with any character an object key may
/// legally contain. Encoding happens once in [`Request::request_target`] and once more
/// identically inside the canonical request, and the two must agree byte for byte or the
/// signature is over a different request than the one sent.
#[derive(Debug)]
pub struct Request<'a> {
    /// The verb.
    pub method: Method,
    /// The unencoded path, starting with `/`.
    pub path: String,
    /// Query parameters, unencoded, in any order.
    pub query: Vec<(String, String)>,
    /// Headers, in any order and any case. Every one of them is signed.
    pub headers: Vec<(String, String)>,
    /// The body. Borrowed: an 8 MiB SST is not copied to be sent.
    pub body: &'a [u8],
}

impl<'a> Request<'a> {
    /// A request with no query, no headers and no body.
    pub fn new(method: Method, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: &[],
        }
    }

    /// Adds a header. Later duplicates are kept: the canonical request signs all of them.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Adds a query parameter.
    #[must_use]
    pub fn param(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((name.into(), value.into()));
        self
    }

    /// Attaches a body.
    #[must_use]
    pub fn with_body(mut self, body: &'a [u8]) -> Self {
        self.body = body;
        self
    }

    /// The request target: the encoded path, then the sorted encoded query.
    ///
    /// Sorted, even though HTTP does not require it, because the canonical request sorts and
    /// the two strings have to be the same one. That is not a coincidence to rely on quietly,
    /// so both call [`crate::sigv4::uri_encode`] and both sort.
    #[must_use]
    pub fn request_target(&self) -> String {
        let path = crate::sigv4::uri_encode(&self.path, false);
        if self.query.is_empty() {
            return path;
        }
        let mut params: Vec<(String, String)> = self
            .query
            .iter()
            .map(|(name, value)| {
                (
                    crate::sigv4::uri_encode(name, true),
                    crate::sigv4::uri_encode(value, true),
                )
            })
            .collect();
        params.sort();
        let query = params
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        format!("{path}?{query}")
    }

    /// The bytes to put on the wire.
    ///
    /// `Connection: keep-alive` since ADR 0039. It is redundant on HTTP/1.1, where persistence is
    /// the default, and it is written anyway because a proxy or an HTTP/1.0 endpoint in the middle
    /// reads it — and because the header is what makes the intent visible in a packet capture.
    ///
    /// **Not signed.** Every header in `self.headers` goes into the canonical request; this one is
    /// appended after them and is a hop-by-hop header, which `SigV4` does not cover. That is why
    /// changing it here changed no signature.
    #[must_use]
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512 + self.body.len());
        out.extend_from_slice(self.method.as_str().as_bytes());
        out.push(b' ');
        out.extend_from_slice(self.request_target().as_bytes());
        out.extend_from_slice(b" HTTP/1.1\r\n");
        for (name, value) in &self.headers {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
        out.extend_from_slice(self.body);
        out
    }
}

/// A parsed response.
#[derive(Debug, Clone)]
pub struct Response {
    /// The status code from the status line.
    pub status: u16,
    /// Headers, with names lowercased so lookup is a comparison and not a search.
    pub headers: Vec<(String, String)>,
    /// The body, however it was delimited.
    pub body: Vec<u8>,
}

impl Response {
    /// The first value of `name`, which must be given in lowercase.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the status is 2xx.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether the connection this arrived on may carry another request.
    ///
    /// Three things say no, and the first two are the ones that matter:
    ///
    /// * **`Connection: close`.** The server has said this is the last exchange. The value is a
    ///   comma-separated list of tokens, so it is searched rather than compared.
    /// * **No framing.** A response with neither `Content-Length` nor `Transfer-Encoding: chunked`
    ///   is delimited by the close itself, so there is nothing left to reuse. This is also the case
    ///   an HTTP/1.0 endpoint without keep-alive produces, which is why the version is not needed
    ///   here to reach the right answer.
    /// * a status that carries no body is exempt from the framing rule, because `204` and `304`
    ///   have no body to delimit and are perfectly reusable.
    ///
    /// Erring towards `false` costs a connection; erring towards `true` costs a request sent into
    /// a socket the peer has finished with.
    #[must_use]
    pub fn may_reuse_connection(&self) -> bool {
        let closing = self
            .headers
            .iter()
            .filter(|(name, _)| name == "connection")
            .any(|(_, value)| {
                value
                    .split(',')
                    .any(|token| token.trim().eq_ignore_ascii_case("close"))
            });
        if closing {
            return false;
        }
        if matches!(self.status, 204 | 304) || (100..200).contains(&self.status) {
            return true;
        }
        self.headers.iter().any(|(name, value)| {
            name == "content-length"
                || (name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"))
        })
    }

    /// The body as text, truncated, for an error message.
    ///
    /// S3 error bodies are XML with a `<Code>` in them. We do not parse XML — the status is
    /// what the client branches on — but the body is exactly what an operator needs to see, so
    /// it goes in the error verbatim and shortened.
    #[must_use]
    pub fn body_excerpt(&self) -> String {
        const LIMIT: usize = 512;
        let text = String::from_utf8_lossy(&self.body);
        let trimmed = text.trim();
        if trimmed.len() <= LIMIT {
            return trimmed.to_string();
        }
        let mut end = LIMIT;
        while end > 0 && !trimmed.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &trimmed[..end])
    }
}

/// Reads one complete response from `reader`.
///
/// This is the function that is fuzzed. It must terminate, must not panic, and must not
/// allocate more than [`MAX_BODY_BYTES`] whatever the bytes say.
pub fn read_response<R: Read>(reader: &mut R) -> Result<Response> {
    let (head, mut leftover) = read_head(reader)?;
    let (status, headers) = parse_head(&head)?;

    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map(|(_, value)| value.trim().parse::<u64>())
        .transpose()
        .map_err(|_| Error::MalformedResponse("Content-Length is not a number".into()))?;

    // RFC 9110: these statuses have no body, whatever the headers claim. A `204` carrying a
    // `Content-Length` would otherwise make us wait for bytes that never come.
    let body = if matches!(status, 204 | 304) || (100..200).contains(&status) {
        Vec::new()
    } else if chunked {
        read_chunked(reader, &mut leftover)?
    } else if let Some(length) = content_length {
        let length = usize::try_from(length)
            .ok()
            .filter(|length| *length <= MAX_BODY_BYTES)
            .ok_or_else(|| {
                Error::MalformedResponse(format!("Content-Length {length} is beyond the limit"))
            })?;
        read_exactly(reader, &mut leftover, length)?
    } else {
        // No framing at all: the body is whatever arrives before the connection closes, which
        // is legal in HTTP/1.1 for a response and is what `Connection: close` invites.
        read_to_end(reader, &mut leftover)?
    };

    Ok(Response {
        status,
        headers,
        body,
    })
}

/// Reads up to and including the blank line that ends the headers, returning the head and
/// whatever of the body arrived in the same reads.
fn read_head<R: Read>(reader: &mut R) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    let mut searched = 0usize;
    loop {
        // The terminator can straddle two reads, so rescanning starts three bytes back.
        if let Some(at) = find(&buffer[searched..], b"\r\n\r\n") {
            let end = searched + at + 4;
            let leftover = buffer[end..].to_vec();
            buffer.truncate(end);
            return Ok((buffer, leftover));
        }
        searched = buffer.len().saturating_sub(3);

        if buffer.len() > MAX_HEADER_BYTES {
            return Err(Error::MalformedResponse(format!(
                "no end of headers within {MAX_HEADER_BYTES} bytes"
            )));
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|source| Error::io("reading a response", source))?;
        if read == 0 {
            return Err(Error::MalformedResponse(
                "the connection closed before the headers ended".into(),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Splits the head into a status and lowercased headers.
fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>)> {
    let text = std::str::from_utf8(head)
        .map_err(|_| Error::MalformedResponse("the response head is not UTF-8".into()))?;
    let mut lines = text.split("\r\n");

    let status_line = lines
        .next()
        .ok_or_else(|| Error::MalformedResponse("empty response".into()))?;
    let status = parse_status_line(status_line)?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            // Obsolete line folding. RFC 9112 says a recipient may reject it, and joining it
            // silently is how a header means one thing to us and another to the server.
            return Err(Error::MalformedResponse(
                "a folded header line, which this parser rejects".into(),
            ));
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            Error::MalformedResponse(format!(
                "a header line without a colon: {:?}",
                excerpt(line)
            ))
        })?;
        if name.is_empty() || name.contains(' ') {
            return Err(Error::MalformedResponse(format!(
                "not a header name: {:?}",
                excerpt(name)
            )));
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok((status, headers))
}

/// `HTTP/1.1 200 OK` — the version, three digits, and a reason we ignore.
fn parse_status_line(line: &str) -> Result<u16> {
    let malformed =
        || Error::MalformedResponse(format!("not an HTTP status line: {:?}", excerpt(line)));
    let (version, rest) = line.split_once(' ').ok_or_else(malformed)?;
    if !version.starts_with("HTTP/1.") {
        return Err(malformed());
    }
    let code = rest.split(' ').next().unwrap_or(rest);
    if code.len() != 3 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }
    code.parse().map_err(|_| malformed())
}

/// Reads exactly `want` more body bytes, counting what already arrived.
fn read_exactly<R: Read>(reader: &mut R, leftover: &mut Vec<u8>, want: usize) -> Result<Vec<u8>> {
    if leftover.len() >= want {
        let rest = leftover.split_off(want);
        let body = std::mem::replace(leftover, rest);
        return Ok(body);
    }
    let mut body = std::mem::take(leftover);
    body.reserve(want - body.len());
    // On the heap: a 32 KiB stack buffer in a function the uploader thread calls is a
    // meaningful fraction of a thread's stack, and clippy is right to say so.
    let mut chunk = vec![0u8; 32 * 1024];
    while body.len() < want {
        let read = reader
            .read(&mut chunk)
            .map_err(|source| Error::io("reading a response body", source))?;
        if read == 0 {
            return Err(Error::MalformedResponse(format!(
                "the connection closed {} bytes short of the declared length",
                want - body.len()
            )));
        }
        let take = read.min(want - body.len());
        body.extend_from_slice(&chunk[..take]);
    }
    Ok(body)
}

/// Reads until the peer closes, bounded.
fn read_to_end<R: Read>(reader: &mut R, leftover: &mut Vec<u8>) -> Result<Vec<u8>> {
    let mut body = std::mem::take(leftover);
    let mut chunk = vec![0u8; 32 * 1024];
    loop {
        if body.len() > MAX_BODY_BYTES {
            return Err(Error::MalformedResponse(format!(
                "an unframed body longer than {MAX_BODY_BYTES} bytes"
            )));
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|source| Error::io("reading a response body", source))?;
        if read == 0 {
            return Ok(body);
        }
        body.extend_from_slice(&chunk[..read]);
    }
}

/// `Transfer-Encoding: chunked`, which `MinIO` uses for some listings.
fn read_chunked<R: Read>(reader: &mut R, leftover: &mut Vec<u8>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let line = read_line(reader, leftover)?;
        // A chunk size may carry `;extensions`, which we ignore but must not choke on.
        let size_text = line.split(';').next().unwrap_or(&line).trim();
        let size = usize::from_str_radix(size_text, 16).map_err(|_| {
            Error::MalformedResponse(format!("not a chunk size: {:?}", excerpt(size_text)))
        })?;
        if body.len().saturating_add(size) > MAX_BODY_BYTES {
            return Err(Error::MalformedResponse(
                "a chunked body longer than the limit".into(),
            ));
        }
        if size == 0 {
            // Trailers, then the final blank line. Read until one is empty; a peer that never
            // sends it hits the header limit rather than looping forever.
            let mut trailer_bytes = 0usize;
            loop {
                let trailer = read_line(reader, leftover)?;
                if trailer.is_empty() {
                    break;
                }
                trailer_bytes += trailer.len();
                if trailer_bytes > MAX_HEADER_BYTES {
                    return Err(Error::MalformedResponse("trailers without an end".into()));
                }
            }
            return Ok(body);
        }
        let chunk = read_exactly(reader, leftover, size)?;
        body.extend_from_slice(&chunk);
        let terminator = read_line(reader, leftover)?;
        if !terminator.is_empty() {
            return Err(Error::MalformedResponse(
                "a chunk not followed by CRLF".into(),
            ));
        }
    }
}

/// One CRLF-terminated line, as text.
fn read_line<R: Read>(reader: &mut R, leftover: &mut Vec<u8>) -> Result<String> {
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(at) = find(leftover, b"\r\n") {
            let rest = leftover.split_off(at + 2);
            let mut line = std::mem::replace(leftover, rest);
            line.truncate(at);
            return String::from_utf8(line)
                .map_err(|_| Error::MalformedResponse("a chunk header that is not UTF-8".into()));
        }
        if leftover.len() > MAX_HEADER_BYTES {
            return Err(Error::MalformedResponse(
                "a chunk header without an end".into(),
            ));
        }
        let read = reader
            .read(&mut chunk)
            .map_err(|source| Error::io("reading a chunk header", source))?;
        if read == 0 {
            return Err(Error::MalformedResponse(
                "the connection closed inside a chunked body".into(),
            ));
        }
        leftover.extend_from_slice(&chunk[..read]);
    }
}

/// The first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A short, printable prefix for an error message about bytes we did not write.
fn excerpt(text: &str) -> String {
    let mut end = 64.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{MAX_BODY_BYTES, Method, Request, Response, read_response};
    use crate::error::Error;

    fn parse(bytes: &[u8]) -> Result<Response, Error> {
        read_response(&mut &bytes[..])
    }

    #[test]
    fn a_put_serialises_to_what_was_asked_for() {
        let body = b"sst bytes";
        let request = Request::new(Method::Put, "/esker/tier/000007.sst")
            .header("Host", "localhost:9000")
            .header("Content-Length", "9")
            .with_body(body);
        let wire = String::from_utf8(request.serialize()).unwrap();
        assert!(
            wire.starts_with("PUT /esker/tier/000007.sst HTTP/1.1\r\n"),
            "{wire}"
        );
        assert!(wire.contains("Host: localhost:9000\r\n"));
        assert!(wire.ends_with("\r\n\r\nsst bytes"));
    }

    /// The request target and the canonical request must encode identically, or the signature
    /// covers a different request than the one on the wire — which produces a 403 that says
    /// nothing about why.
    #[test]
    fn the_request_target_matches_the_canonical_request() {
        let request = Request::new(Method::Get, "/esker/a b/000007.sst")
            .param("list-type", "2")
            .param("continuation-token", "x/y+z=");
        let target = request.request_target();
        assert_eq!(
            target,
            "/esker/a%20b/000007.sst?continuation-token=x%2Fy%2Bz%3D&list-type=2"
        );

        let canonical = crate::sigv4::CanonicalRequest::new(
            Method::Get.as_str(),
            &request.path,
            &request.query,
            &[("host".to_string(), "h".to_string())],
            esker_base::sha256::EMPTY_DIGEST_HEX,
        );
        let lines: Vec<&str> = canonical.text.lines().collect();
        let rebuilt = format!("{}?{}", lines[1], lines[2]);
        assert_eq!(rebuilt, target);
    }

    #[test]
    fn a_content_length_body_parses() {
        let response =
            parse(b"HTTP/1.1 200 OK\r\nETag: \"abc\"\r\nContent-Length: 5\r\n\r\nhello").unwrap();
        assert_eq!(response.status, 200);
        assert!(response.is_success());
        assert_eq!(response.body, b"hello");
        assert_eq!(response.header("etag"), Some("\"abc\""));
        // Names are lowercased on the way in, so lookup never has to search case-insensitively.
        assert_eq!(response.header("ETag"), None);
    }

    #[test]
    fn a_chunked_body_parses_including_its_extensions_and_trailers() {
        let wire = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                     5;ext=1\r\nhello\r\n\
                     6\r\n world\r\n\
                     0\r\nX-Trailer: ignored\r\n\r\n";
        let response = parse(wire).unwrap();
        assert_eq!(response.body, b"hello world");
    }

    /// `Transfer-Encoding` wins over `Content-Length` when a server sends both — and a server
    /// that sends both is doing something we should not try to reconcile.
    #[test]
    fn chunked_wins_over_content_length() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nTransfer-Encoding: chunked\r\n\r\n\
                     2\r\nhi\r\n0\r\n\r\n";
        assert_eq!(parse(wire).unwrap().body, b"hi");
    }

    /// A `204` has no body by definition. Believing a `Content-Length` here means blocking on
    /// bytes that are never sent, which is a hang rather than an error — much worse.
    #[test]
    fn a_204_has_no_body_whatever_it_claims() {
        let response = parse(b"HTTP/1.1 204 No Content\r\nContent-Length: 10\r\n\r\n").unwrap();
        assert_eq!(response.status, 204);
        assert!(response.body.is_empty());
    }

    #[test]
    fn an_unframed_body_runs_to_the_close() {
        let response = parse(b"HTTP/1.1 200 OK\r\n\r\nwhatever arrives").unwrap();
        assert_eq!(response.body, b"whatever arrives");
    }

    /// Every truncation of a valid response is an error, never a panic and never a success
    /// with a short body. This is the shape a connection reset actually takes.
    #[test]
    fn every_prefix_of_a_valid_response_is_an_error_not_a_panic() {
        let wire = b"HTTP/1.1 206 Partial Content\r\n\
                     Content-Range: bytes 0-4/11\r\n\
                     ETag: \"abc\"\r\n\
                     Content-Length: 5\r\n\r\nhello";
        for cut in 0..wire.len() {
            let result = parse(&wire[..cut]);
            assert!(
                result.is_err(),
                "a {cut}-byte prefix parsed as a whole response"
            );
        }
        assert!(parse(wire).is_ok());
    }

    #[test]
    fn the_malformed_cases_are_errors_with_something_to_read() {
        let cases: &[(&[u8], &str)] = &[
            (b"", "closed before the headers"),
            (b"NOT HTTP AT ALL\r\n\r\n", "status line"),
            (b"HTTP/1.1 20 OK\r\n\r\n", "status line"),
            (b"HTTP/1.1 2000 OK\r\n\r\n", "status line"),
            (b"HTTP/2.0 200 OK\r\n\r\n", "status line"),
            (
                b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
                "without a colon",
            ),
            (b"HTTP/1.1 200 OK\r\n\tfolded: yes\r\n\r\n", "folded"),
            (
                b"HTTP/1.1 200 OK\r\nBad Name: v\r\n\r\n",
                "not a header name",
            ),
            (
                b"HTTP/1.1 200 OK\r\nContent-Length: banana\r\n\r\n",
                "not a number",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                "not a chunk size",
            ),
            (
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhiXX\r\n",
                "not followed by CRLF",
            ),
        ];
        for (wire, expected) in cases {
            let err = parse(wire).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "{:?} gave {err}, wanted something about {expected}",
                String::from_utf8_lossy(wire)
            );
        }
    }

    /// The length is a number a stranger chose, so it is checked against the limit *before*
    /// anything is reserved. A parser that reserves first is a parser that can be made to
    /// allocate four gigabytes by one header.
    #[test]
    fn an_absurd_content_length_is_refused_before_allocating() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551615\r\n\r\n";
        let err = parse(wire).unwrap_err();
        assert!(err.to_string().contains("beyond the limit"), "{err}");

        let wire = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        let err = parse(wire.as_bytes()).unwrap_err();
        assert!(err.to_string().contains("beyond the limit"), "{err}");
    }

    #[test]
    fn a_header_block_without_an_end_is_bounded() {
        let mut wire = b"HTTP/1.1 200 OK\r\n".to_vec();
        while wire.len() < super::MAX_HEADER_BYTES + 4096 {
            wire.extend_from_slice(b"X-Padding: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n");
        }
        let err = parse(&wire).unwrap_err();
        assert!(err.to_string().contains("no end of headers"), "{err}");
    }

    /// The terminator can straddle two reads. A reader that hands over one byte at a time is
    /// the cheapest way to prove the rescan window is right.
    #[test]
    fn the_header_terminator_may_straddle_reads() {
        struct OneByteAtATime<'a>(&'a [u8]);
        impl std::io::Read for OneByteAtATime<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.0.is_empty() || buf.is_empty() {
                    return Ok(0);
                }
                buf[0] = self.0[0];
                self.0 = &self.0[1..];
                Ok(1)
            }
        }
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let response = read_response(&mut OneByteAtATime(wire)).unwrap();
        assert_eq!(response.body, b"hello");
    }

    #[test]
    fn the_body_excerpt_truncates_on_a_character_boundary() {
        let response = Response {
            status: 500,
            headers: Vec::new(),
            body: "é".repeat(400).into_bytes(),
        };
        let excerpt = response.body_excerpt();
        assert!(excerpt.ends_with('…'));
        assert!(excerpt.len() <= 512 + 3);
    }

    proptest::proptest! {
        /// Arbitrary bytes are an error or a response, never a panic and never a hang.
        /// `CLAUDE.md` invariant 9, applied to the one parser in this crate that reads bytes
        /// somebody else wrote.
        #[test]
        fn arbitrary_bytes_never_panic(bytes: Vec<u8>) {
            let _ = parse(&bytes);
        }

        /// The same, seeded with a valid prefix so the generator actually reaches the body
        /// parsers rather than dying on the status line every time.
        #[test]
        fn arbitrary_bodies_after_a_valid_head_never_panic(
            tail: Vec<u8>,
            chunked: bool,
            length in 0u64..1_000_000,
        ) {
            let head = if chunked {
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_string()
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n")
            };
            let mut wire = head.into_bytes();
            wire.extend_from_slice(&tail);
            let _ = parse(&wire);
        }
    }
}
