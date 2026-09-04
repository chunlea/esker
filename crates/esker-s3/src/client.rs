//! The four calls, signed and sent.
//!
//! `PutObject`, `GetObject` (whole or ranged), `ListObjectsV2`, `DeleteObject`. That is the
//! entire surface SST tiering needs, and adding a fifth should require a reason.
//!
//! Every request is built the same way — path, query, headers, payload hash — signed by
//! [`crate::sigv4`], serialised by [`crate::http`], and handed to a [`Transport`]. The one
//! place with real logic beyond that is [`S3Client::get_range`], which has to prove the server
//! answered with the bytes it was asked for
//! ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 6).

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use esker_base::sha256;

use crate::error::{Error, Result};
use crate::http::{Method, Request, Response};
use crate::sigv4::{self, CanonicalRequest, Credentials, Scope};
use crate::transport::{TcpTransport, Transport};
use crate::xml;
use crate::{GetResponse, MAX_SINGLE_PUT, ObjectStore, ObjectSummary, PutOutcome};

/// Where the object store is, on the network.
///
/// Parsed from a URL rather than taken as a host and a port so that a misconfiguration is
/// caught once, here, with a message — instead of becoming a connection refused on a port
/// nobody meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// The host to connect to and to put in the `Host` header.
    pub host: String,
    /// The port.
    pub port: u16,
    /// Whether to speak TLS to it.
    ///
    /// Set only by parsing an `https://` URL, and only in a build with the `tls` feature — so a
    /// client cannot be pointed at an encrypted endpoint by a build that has no encryption.
    pub tls: bool,
}

impl Endpoint {
    /// Parses `http://host[:port]`, or `https://host[:port]` with the `tls` feature.
    ///
    /// **Without the feature, `https://` is refused**, and that refusal is the whole point:
    /// accepting it and speaking plaintext anyway would be the worst outcome available — a
    /// configuration that looks encrypted, is not, and gives no sign of it (ADR 0025, and the same
    /// rule the PostgreSQL port follows for `--tls-cert` in ADR 0055).
    pub fn parse(url: &str) -> Result<Self> {
        let (rest, tls) = if let Some(rest) = url.strip_prefix("https://") {
            #[cfg(not(feature = "tls"))]
            {
                let _ = rest;
                return Err(Error::Config(format!(
                    "{url}: this build has no TLS, so an https endpoint would silently be spoken \
                     in plaintext. Rebuild with `--features tls`, or use http:// to a MinIO or a \
                     local TLS terminator — docs/adr/0055-the-tls-options-across-three-surfaces-\
                     measured.md is the decision and 0025 is its history"
                )));
            }
            #[cfg(feature = "tls")]
            (rest, true)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (rest, false)
        } else {
            return Err(Error::Config(format!(
                "{url}: an endpoint must start with http:// or https://"
            )));
        };

        let authority = rest.split(['/', '?']).next().unwrap_or(rest);
        if authority.is_empty() {
            return Err(Error::Config(format!("{url}: no host")));
        }
        // Only the last colon separates a port, so an IPv6 literal in brackets survives.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port))
                if !host.ends_with(']') || port.chars().all(|c| c.is_ascii_digit()) =>
            {
                let port = port
                    .parse()
                    .map_err(|_| Error::Config(format!("{url}: {port:?} is not a port number")))?;
                (host, port)
            }
            _ => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(Error::Config(format!("{url}: no host")));
        }
        Ok(Self {
            host: host.to_string(),
            port,
            tls,
        })
    }

    /// The `Host` header value: the port is omitted when it is the scheme's default.
    ///
    /// **The scheme decides which default**, and getting that wrong changes the signature: the
    /// `Host` header is a signed header, so `example.com` and `example.com:443` sign differently
    /// and one of them is refused by the server.
    #[must_use]
    pub fn host_header(&self) -> String {
        if self.port == if self.tls { 443 } else { 80 } {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Everything the client needs to make a request.
#[derive(Debug, Clone)]
pub struct Config {
    /// Where to connect.
    pub endpoint: Endpoint,
    /// The bucket every key lives in.
    pub bucket: String,
    /// A prefix prepended to every key, with no leading and exactly one trailing separator
    /// when it is not empty. [`Config::from_store_url`] normalises it.
    pub prefix: String,
    /// The region to sign for. `MinIO` ignores it but must be told the same one every time.
    pub region: String,
    /// The identity to sign with.
    pub credentials: Credentials,
    /// `true` addresses the bucket as a path (`/bucket/key`), which is what `MinIO` wants and
    /// what any S3-compatible endpoint accepts. `false` uses a virtual host
    /// (`bucket.host/key`), which real S3 prefers.
    pub path_style: bool,
    /// Where the request timestamp comes from.
    ///
    /// A function so tests can pin it. `CLAUDE.md` invariant 6 forbids wall clocks for
    /// *ordering*; this is not ordering — `SigV4` requires a timestamp within fifteen minutes of
    /// the server's, and no TSO can supply that.
    pub clock: fn() -> i64,
}

/// Seconds since the Unix epoch, or zero if the clock is before it.
fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

impl Config {
    /// Builds a config from an `s3://bucket/prefix` store URL and an endpoint.
    ///
    /// This is what `--sst-store` is parsed into. The prefix is normalised so that joining it
    /// to a file name is a concatenation and never produces `//` or a missing separator.
    pub fn from_store_url(
        store_url: &str,
        endpoint: Endpoint,
        region: impl Into<String>,
        credentials: Credentials,
    ) -> Result<Self> {
        let rest = store_url.strip_prefix("s3://").ok_or_else(|| {
            Error::Config(format!(
                "{store_url}: an SST store must be s3://bucket/prefix"
            ))
        })?;
        let (bucket, prefix) = match rest.split_once('/') {
            Some((bucket, prefix)) => (bucket, prefix),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            return Err(Error::Config(format!("{store_url}: no bucket")));
        }
        if bucket.contains(['?', '#', ':', ' ']) {
            return Err(Error::Config(format!(
                "{store_url}: {bucket:?} is not a bucket name"
            )));
        }
        let prefix = prefix.trim_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };

        Ok(Self {
            endpoint,
            bucket: bucket.to_string(),
            prefix,
            region: region.into(),
            credentials,
            path_style: true,
            clock: unix_now,
        })
    }

    /// The full object key for a name under the configured prefix.
    #[must_use]
    pub fn key_for(&self, name: &str) -> String {
        format!("{}{}", self.prefix, name.trim_start_matches('/'))
    }
}

/// A client for one bucket.
#[derive(Debug)]
pub struct S3Client {
    config: Config,
    transport: Arc<dyn Transport>,
}

impl S3Client {
    /// A client over a plain TCP transport.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self::with_transport(config, Arc::new(TcpTransport::new()))
    }

    /// A client over a transport of the caller's choosing — a TLS one, when there is one, or a
    /// recording one in a test.
    #[must_use]
    pub fn with_transport(config: Config, transport: Arc<dyn Transport>) -> Self {
        Self { config, transport }
    }

    /// The configuration this client was built with.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The path a key is addressed by, which depends on the addressing style.
    fn path_for(&self, key: &str) -> String {
        if self.config.path_style {
            format!("/{}/{}", self.config.bucket, key)
        } else {
            format!("/{key}")
        }
    }

    /// The `Host` header, which carries the bucket in virtual-host style — and is signed, so
    /// getting it wrong is a 403 rather than a 404.
    fn host_header(&self) -> String {
        if self.config.path_style {
            self.config.endpoint.host_header()
        } else {
            format!(
                "{}.{}",
                self.config.bucket,
                self.config.endpoint.host_header()
            )
        }
    }

    /// Signs `request` and sends it.
    fn send(
        &self,
        operation: &'static str,
        key: &str,
        mut request: Request<'_>,
    ) -> Result<Response> {
        let payload_hash = sha256::hex(&sha256::digest(request.body));
        let (date, timestamp) = sigv4::format_amz_date((self.config.clock)());

        request.headers.push(("host".into(), self.host_header()));
        request
            .headers
            .push(("x-amz-date".into(), timestamp.clone()));
        request
            .headers
            .push(("x-amz-content-sha256".into(), payload_hash.clone()));
        if let Some(token) = &self.config.credentials.session_token {
            request
                .headers
                .push(("x-amz-security-token".into(), token.clone()));
        }

        let scope = Scope {
            date: &date,
            timestamp: &timestamp,
            region: &self.config.region,
            service: "s3",
        };
        let canonical = CanonicalRequest::new(
            request.method.as_str(),
            &request.path,
            &request.query,
            &request.headers,
            &payload_hash,
        );
        let authorization =
            sigv4::authorization_header(&self.config.credentials, &scope, &canonical);
        request
            .headers
            .push(("authorization".into(), authorization));

        let wire = request.serialize();
        tracing::trace!(operation, key, bytes = wire.len(), "sending an S3 request");
        let response = self.transport.round_trip(
            &self.config.endpoint.host,
            self.config.endpoint.port,
            &wire,
        )?;

        if response.is_success() {
            Ok(response)
        } else {
            Err(Error::Status {
                operation,
                key: key.to_string(),
                status: response.status,
                body: response.body_excerpt(),
            })
        }
    }

    /// A `GetObject`, whole or ranged, without the range checking.
    fn get_object(&self, key: &str, range: Option<(u64, u64)>) -> Result<Response> {
        let mut request = Request::new(Method::Get, self.path_for(key));
        if let Some((offset, len)) = range {
            // An inclusive end, which is what HTTP means by a byte range. A zero-length range
            // is not expressible, and callers never ask for one.
            let last = offset.saturating_add(len.max(1) - 1);
            request = request.header("range", format!("bytes={offset}-{last}"));
        }
        self.send("GetObject", key, request)
    }
}

impl ObjectStore for S3Client {
    fn put(&self, key: &str, body: &[u8]) -> Result<Option<String>> {
        if body.len() > MAX_SINGLE_PUT {
            return Err(Error::Config(format!(
                "{key} is {} bytes, past the {MAX_SINGLE_PUT}-byte single-request limit; \
                 this client has no multipart upload",
                body.len()
            )));
        }
        let request = Request::new(Method::Put, self.path_for(key))
            .header("content-length", body.len().to_string())
            .with_body(body);
        let response = self.send("PutObject", key, request)?;
        Ok(response.header("etag").map(xml::normalise_etag))
    }

    fn put_if_absent(&self, key: &str, body: &[u8]) -> Result<PutOutcome> {
        if body.len() > MAX_SINGLE_PUT {
            return Err(Error::Config(format!(
                "{key} is {} bytes, past the {MAX_SINGLE_PUT}-byte single-request limit; \
                 this client has no multipart upload",
                body.len()
            )));
        }
        // `*` is "any entity tag", so `If-None-Match: *` reads as "only if there is no object
        // here at all". The header is signed like every other, because `Request::headers` is what
        // the canonical request is built from — which is also what stops a proxy stripping it
        // without the signature noticing.
        let request = Request::new(Method::Put, self.path_for(key))
            .header("content-length", body.len().to_string())
            .header("if-none-match", "*")
            .with_body(body);
        match self.send("PutObject", key, request) {
            Ok(response) => Ok(PutOutcome::Stored(
                response.header("etag").map(xml::normalise_etag),
            )),
            // 412 is the precondition failing, which here means exactly one thing. 409 is what S3
            // answers when two conditional writes to one key overlap: the loser is told to retry,
            // and for a claim "somebody else is writing this marker right now" is the same answer
            // as "somebody else has it" — the read-back that follows says who.
            Err(Error::Status { status, .. }) if status == 412 || status == 409 => {
                Ok(PutOutcome::AlreadyThere)
            }
            Err(error) => Err(error),
        }
    }

    fn get(&self, key: &str) -> Result<GetResponse> {
        let response = self.get_object(key, None)?;
        let total = response.body.len() as u64;
        Ok(GetResponse {
            etag: response.header("etag").map(xml::normalise_etag),
            total_size: Some(total),
            body: response.body,
        })
    }

    fn get_range(
        &self,
        key: &str,
        offset: u64,
        len: u64,
        expected_etag: Option<&str>,
    ) -> Result<GetResponse> {
        let response = self.get_object(key, Some((offset, len)))?;
        let etag = response.header("etag").map(xml::normalise_etag);

        // The object changed under us. Whatever came back is bytes from a different file, and
        // the SST's own block checksums would report it three layers up as corruption — so it
        // is caught here, where the message can say what actually happened.
        if let (Some(expected), Some(actual)) = (expected_etag, etag.as_deref())
            && expected != actual
        {
            return Err(Error::RangeMismatch {
                key: key.to_string(),
                detail: format!("etag {actual} is not the {expected} recorded at upload"),
            });
        }

        // A `200` means the server ignored the range and sent the whole object — a
        // TLS-terminating proxy or a gateway that does not implement ranges. That *is* the
        // full-GET fallback ADR 0024 describes, already paid for, so slice it rather than
        // asking again.
        if response.status == 200 {
            let total = response.body.len() as u64;
            let start = offset.min(total);
            let end = start.saturating_add(len).min(total);
            let (start, end) = (usize::try_from(start), usize::try_from(end));
            let (Ok(start), Ok(end)) = (start, end) else {
                return Err(Error::RangeMismatch {
                    key: key.to_string(),
                    detail: "the object is larger than this machine can address".into(),
                });
            };
            tracing::debug!(key, "the endpoint ignored a Range header; slicing locally");
            return Ok(GetResponse {
                body: response.body.get(start..end).unwrap_or_default().to_vec(),
                etag,
                total_size: Some(total),
            });
        }

        if response.status != 206 {
            return Err(Error::RangeMismatch {
                key: key.to_string(),
                detail: format!("a ranged GET answered {}, not 206", response.status),
            });
        }

        let (first, total) =
            parse_content_range(response.header("content-range")).ok_or_else(|| {
                Error::RangeMismatch {
                    key: key.to_string(),
                    detail: format!(
                        "a 206 without a usable Content-Range: {:?}",
                        response.header("content-range")
                    ),
                }
            })?;
        if first != offset {
            return Err(Error::RangeMismatch {
                key: key.to_string(),
                detail: format!("asked for byte {offset}, was answered from byte {first}"),
            });
        }
        // Short is legal only at the end of the object; short anywhere else means the server
        // truncated the range and the caller would silently see a hole.
        let expected_len = len.min(total.saturating_sub(offset));
        if response.body.len() as u64 != expected_len {
            return Err(Error::RangeMismatch {
                key: key.to_string(),
                detail: format!(
                    "asked for {expected_len} bytes at {offset}, got {}",
                    response.body.len()
                ),
            });
        }

        Ok(GetResponse {
            body: response.body,
            etag,
            total_size: Some(total),
        })
    }

    fn list(&self, prefix: &str) -> Result<Vec<ObjectSummary>> {
        let mut summaries = Vec::new();
        let mut token: Option<String> = None;
        // A listing that never stops advancing would spin forever on a server that keeps
        // handing back the same token. Each page must produce a *different* token or finish.
        let mut seen_tokens = Vec::new();
        loop {
            let mut request = Request::new(Method::Get, self.path_for(""))
                .param("list-type", "2")
                .param("prefix", prefix);
            if let Some(token) = &token {
                request = request.param("continuation-token", token.clone());
            }
            let response = self.send("ListObjectsV2", prefix, request)?;
            let body = String::from_utf8_lossy(&response.body).into_owned();
            let (page, next) = xml::parse_list_page(&body);
            summaries.extend(page);

            match next {
                None => return Ok(summaries),
                Some(next) if seen_tokens.contains(&next) => {
                    return Err(Error::MalformedResponse(
                        "the listing repeated a continuation token".into(),
                    ));
                }
                Some(next) => {
                    seen_tokens.push(next.clone());
                    token = Some(next);
                }
            }
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        let request = Request::new(Method::Delete, self.path_for(key));
        match self.send("DeleteObject", key, request) {
            Ok(_) => Ok(()),
            // S3 says deleting an absent key succeeds, but a gateway may not agree. The caller
            // wants the object gone, and it is.
            Err(err) if err.is_not_found() => Ok(()),
            Err(err) => Err(err),
        }
    }
}

/// `bytes 0-4/11` → `(0, 11)`. Anything else is `None`.
fn parse_content_range(header: Option<&str>) -> Option<(u64, u64)> {
    let value = header?.trim();
    let rest = value.strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (first, _last) = range.split_once('-')?;
    // `*` as a total means the server does not know it; we need it, so that is not usable.
    Some((first.trim().parse().ok()?, total.trim().parse().ok()?))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Config, Endpoint, S3Client, parse_content_range};
    use crate::error::Error;
    use crate::http::Response;
    use crate::sigv4::Credentials;
    use crate::transport::Transport;
    use crate::{ObjectStore, Result};
    use std::sync::{Arc, Mutex};

    /// A transport that answers from a script and records what it was asked.
    #[derive(Debug, Default)]
    struct Recorder {
        sent: Mutex<Vec<String>>,
        replies: Mutex<Vec<Response>>,
    }

    impl Recorder {
        fn reply(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Response {
            Response {
                status,
                // Lowercased, because that is what `http::read_response` produces and a
                // fixture that differs from the parser tests the wrong thing.
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_string()))
                    .collect(),
                body: body.to_vec(),
            }
        }

        fn push(&self, response: Response) {
            self.replies.lock().unwrap().push(response);
        }

        fn last_request(&self) -> String {
            self.sent
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl Transport for Recorder {
        fn round_trip(&self, _host: &str, _port: u16, request: &[u8]) -> Result<Response> {
            self.sent
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(request).into_owned());
            let mut replies = self.replies.lock().unwrap();
            if replies.is_empty() {
                return Ok(Self::reply(200, &[], b""));
            }
            Ok(replies.remove(0))
        }
    }

    fn client() -> (S3Client, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        let mut config = Config::from_store_url(
            "s3://esker/tier",
            Endpoint::parse("http://localhost:9000").unwrap(),
            "us-east-1",
            Credentials::new("AKIDEXAMPLE", "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"),
        )
        .unwrap();
        // 2015-08-30T12:36:00Z, so the request is byte-stable and the goldens above apply.
        config.clock = || 1_440_938_160;
        (S3Client::with_transport(config, recorder.clone()), recorder)
    }

    /// Without the feature, an `https://` endpoint is a refusal that names the way out — never a
    /// silent plain-HTTP fallback to a port an operator believes is encrypted.
    #[cfg(not(feature = "tls"))]
    #[test]
    fn an_https_endpoint_is_refused_with_a_reason() {
        let err = Endpoint::parse("https://s3.amazonaws.com").unwrap_err();
        assert!(err.to_string().contains("no TLS"), "{err}");
        assert!(err.to_string().contains("--features tls"), "{err}");
        assert!(
            !err.is_retryable(),
            "a misconfiguration is not worth retrying"
        );
    }

    /// With the feature, it parses — and carries the scheme, which decides the default port and
    /// therefore the `Host` header the signature covers.
    #[cfg(feature = "tls")]
    #[test]
    fn an_https_endpoint_parses_and_defaults_to_443() {
        let endpoint = Endpoint::parse("https://s3.amazonaws.com").unwrap();
        assert_eq!(
            endpoint,
            Endpoint {
                host: "s3.amazonaws.com".into(),
                port: 443,
                tls: true,
            }
        );
        // 443 is https's default, so it is omitted; on http it is not the default and stays.
        assert_eq!(endpoint.host_header(), "s3.amazonaws.com");
        assert_eq!(
            Endpoint::parse("https://minio.internal:9443")
                .unwrap()
                .host_header(),
            "minio.internal:9443"
        );
        assert_eq!(
            Endpoint::parse("http://minio.internal:443")
                .unwrap()
                .host_header(),
            "minio.internal:443",
            "443 is not http's default and must stay in the signed Host header"
        );
    }

    #[test]
    fn endpoints_parse_or_say_why_not() {
        assert_eq!(
            Endpoint::parse("http://localhost:9000").unwrap(),
            Endpoint {
                host: "localhost".into(),
                port: 9000,
                tls: false,
            }
        );
        let bare = Endpoint::parse("http://minio.internal").unwrap();
        assert_eq!(bare.port, 80);
        assert_eq!(bare.host_header(), "minio.internal", "port 80 is implied");
        assert_eq!(Endpoint::parse("http://h:9000/ignored").unwrap().host, "h");
        assert!(Endpoint::parse("localhost:9000").is_err());
        assert!(Endpoint::parse("http://").is_err());
        assert!(Endpoint::parse("http://h:not-a-port").is_err());
    }

    #[test]
    fn a_store_url_becomes_a_bucket_and_a_normalised_prefix() {
        let endpoint = Endpoint::parse("http://localhost:9000").unwrap();
        let credentials = Credentials::new("a", "b");
        let of = |url: &str| {
            Config::from_store_url(url, endpoint.clone(), "r", credentials.clone()).map(|config| {
                let key = config.key_for("000007.sst");
                (config.bucket, config.prefix, key)
            })
        };
        assert_eq!(
            of("s3://esker/tier").unwrap(),
            ("esker".into(), "tier/".into(), "tier/000007.sst".into())
        );
        assert_eq!(
            of("s3://esker/a/b/").unwrap(),
            ("esker".into(), "a/b/".into(), "a/b/000007.sst".into())
        );
        assert_eq!(
            of("s3://esker").unwrap(),
            ("esker".into(), String::new(), "000007.sst".into())
        );
        assert!(of("esker/tier").is_err(), "no scheme");
        assert!(of("s3:///tier").is_err(), "no bucket");
        assert!(of("s3://a b/tier").is_err(), "not a bucket name");
    }

    #[test]
    fn a_put_is_signed_and_addressed_path_style() {
        // SHA-256("sst"), computed independently of this crate.
        const PAYLOAD_SHA: &str =
            "7b8932a7d01817e6e54e4d39151138caaaf577726f29b8a627bfc55da0de2588";
        let (client, recorder) = client();
        recorder.push(Recorder::reply(200, &[("ETag", "\"deadbeef\"")], b""));
        let etag = client.put("tier/000007.sst", b"sst").unwrap();
        assert_eq!(etag.as_deref(), Some("deadbeef"), "quotes are stripped");

        let sent = recorder.last_request();
        assert!(
            sent.starts_with("PUT /esker/tier/000007.sst HTTP/1.1\r\n"),
            "{sent}"
        );
        assert!(sent.contains("host: localhost:9000\r\n"), "{sent}");
        assert!(sent.contains("x-amz-date: 20150830T123600Z\r\n"), "{sent}");
        // The payload hash is signed and sent; S3 rejects a request without it. The value is
        // SHA-256("sst"), independently computed — a client that hashed the wrong thing would
        // still produce a consistent-looking header.
        assert!(
            sent.contains(&format!("x-amz-content-sha256: {PAYLOAD_SHA}\r\n")),
            "{sent}"
        );
        assert!(
            sent.contains("authorization: AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/s3/aws4_request"),
            "{sent}"
        );
        assert!(sent.ends_with("\r\n\r\nsst"), "{sent}");
    }

    #[test]
    fn an_object_too_large_for_one_put_is_an_error_not_a_truncation() {
        let (client, _) = client();
        let body = vec![0u8; crate::MAX_SINGLE_PUT + 1];
        let err = client.put("k", &body).unwrap_err();
        assert!(err.to_string().contains("multipart"), "{err}");
    }

    #[test]
    fn a_ranged_get_checks_the_content_range() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            206,
            &[("Content-Range", "bytes 3-6/11"), ("ETag", "\"v1\"")],
            b"3456",
        ));
        let got = client
            .get_range("tier/000007.sst", 3, 4, Some("v1"))
            .unwrap();
        assert_eq!(got.body, b"3456");
        assert_eq!(got.total_size, Some(11));
        assert!(recorder.last_request().contains("range: bytes=3-6\r\n"));
    }

    #[test]
    fn a_range_answered_from_the_wrong_offset_is_caught() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            206,
            &[("Content-Range", "bytes 0-3/11")],
            b"0123",
        ));
        let err = client.get_range("k", 3, 4, None).unwrap_err();
        assert!(err.to_string().contains("answered from byte 0"), "{err}");
        assert!(err.is_retryable());
    }

    #[test]
    fn a_short_range_that_is_not_at_the_end_is_caught() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            206,
            &[("Content-Range", "bytes 0-3/1000")],
            b"01",
        ));
        let err = client.get_range("k", 0, 4, None).unwrap_err();
        assert!(err.to_string().contains("got 2"), "{err}");
    }

    /// A range that runs off the end of the object is legal and returns what remains — the
    /// same contract `RandomAccessFile::read_at` has at EOF.
    #[test]
    fn a_range_past_the_end_returns_what_remains() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            206,
            &[("Content-Range", "bytes 8-10/11")],
            b"890",
        ));
        let got = client.get_range("k", 8, 100, None).unwrap();
        assert_eq!(got.body, b"890");
    }

    /// A gateway that ignores `Range` answers 200 with the whole object. That is the full-GET
    /// fallback already paid for, so it is sliced rather than re-requested.
    #[test]
    fn a_200_to_a_ranged_request_is_sliced_locally() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(200, &[("ETag", "\"v1\"")], b"0123456789"));
        let got = client.get_range("k", 3, 4, Some("v1")).unwrap();
        assert_eq!(got.body, b"3456");
        assert_eq!(got.total_size, Some(10));
    }

    #[test]
    fn an_etag_that_changed_since_the_upload_is_a_range_mismatch() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            206,
            &[("Content-Range", "bytes 0-3/11"), ("ETag", "\"v2\"")],
            b"0123",
        ));
        let err = client.get_range("k", 0, 4, Some("v1")).unwrap_err();
        assert!(err.to_string().contains("recorded at upload"), "{err}");
    }

    #[test]
    fn a_non_2xx_carries_the_status_and_the_body() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            403,
            &[],
            b"<Error><Code>SignatureDoesNotMatch</Code></Error>",
        ));
        let err = client.get("k").unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert!(err.to_string().contains("SignatureDoesNotMatch"), "{err}");
        assert!(!err.is_retryable(), "a 403 is a standing misconfiguration");
    }

    #[test]
    fn deleting_something_that_is_not_there_succeeds() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(404, &[], b""));
        assert!(client.delete("gone").is_ok());
    }

    #[test]
    fn content_ranges_parse_or_do_not() {
        assert_eq!(parse_content_range(Some("bytes 0-4/11")), Some((0, 11)));
        assert_eq!(
            parse_content_range(Some(" bytes 12-20/99 ")),
            Some((12, 99))
        );
        assert_eq!(parse_content_range(Some("bytes 0-4/*")), None);
        assert_eq!(parse_content_range(Some("items 0-4/11")), None);
        assert_eq!(parse_content_range(Some("nonsense")), None);
        assert_eq!(parse_content_range(None), None);
    }

    #[test]
    fn a_listing_that_repeats_its_token_is_refused_rather_than_looped() {
        let (client, recorder) = client();
        let page = b"<ListBucketResult><IsTruncated>true</IsTruncated>\
                     <NextContinuationToken>same</NextContinuationToken></ListBucketResult>";
        for _ in 0..3 {
            recorder.push(Recorder::reply(200, &[], page));
        }
        let err = client.list("tier/").unwrap_err();
        assert!(matches!(err, Error::MalformedResponse(_)), "{err}");
        assert!(
            err.to_string().contains("repeated a continuation token"),
            "{err}"
        );
    }

    #[test]
    fn a_listing_follows_its_continuation_token() {
        let (client, recorder) = client();
        recorder.push(Recorder::reply(
            200,
            &[],
            b"<ListBucketResult><IsTruncated>true</IsTruncated>\
              <NextContinuationToken>page2</NextContinuationToken>\
              <Contents><Key>a</Key><Size>1</Size><ETag>&quot;1&quot;</ETag></Contents>\
              </ListBucketResult>",
        ));
        recorder.push(Recorder::reply(
            200,
            &[],
            b"<ListBucketResult><IsTruncated>false</IsTruncated>\
              <Contents><Key>b</Key><Size>2</Size><ETag>&quot;2&quot;</ETag></Contents>\
              </ListBucketResult>",
        ));
        let listed = client.list("tier/").unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[1].key, "b");
        assert!(
            recorder.last_request().contains("continuation-token=page2"),
            "the second request must carry the token"
        );
    }
}
