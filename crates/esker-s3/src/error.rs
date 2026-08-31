//! What can go wrong between here and a bucket.
//!
//! The distinction that matters to callers is [`Error::is_retryable`]. `esker-engine`'s
//! uploader retries forever ([ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md)
//! decision 2), so it needs to know which failures are worth waiting on — a connection refused
//! while `MinIO` restarts — and which are a standing misconfiguration that no amount of waiting
//! fixes, such as a signature the server will not accept.

use std::fmt;

/// Anything that can stop an S3 call from returning bytes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The endpoint could not be reached, or the connection died mid-exchange.
    #[error("{operation}: {source}")]
    Io {
        /// Which call was in flight.
        operation: &'static str,
        /// The underlying failure.
        source: std::io::Error,
    },

    /// The server answered, but not with anything HTTP-shaped.
    ///
    /// This is the parser refusing to guess. Every variant of it is an error value and never a
    /// panic, which is what `CLAUDE.md` invariant 9 asks of a parser fed bytes we did not
    /// write.
    #[error("malformed HTTP response: {0}")]
    MalformedResponse(String),

    /// The server answered with a status the call does not accept.
    #[error("{operation} {key}: HTTP {status}{}", ShowBody(.body))]
    Status {
        /// Which call.
        operation: &'static str,
        /// The object key it was about.
        key: String,
        /// The status line's code.
        status: u16,
        /// Whatever error document came with it, truncated.
        body: String,
    },

    /// A ranged GET did not return the bytes it was asked for.
    ///
    /// [ADR 0024](../../../docs/adr/0024-tiering-failure-semantics.md) decision 6: a range that
    /// silently answers from somewhere else, or from a different version of the object, has to
    /// be caught here rather than surfacing as a checksum failure three layers up.
    #[error("range check failed for {key}: {detail}")]
    RangeMismatch {
        /// The object key.
        key: String,
        /// What did not line up.
        detail: String,
    },

    /// The configuration cannot be used as given.
    #[error("{0}")]
    Config(String),
}

impl Error {
    /// Whether waiting and trying again could plausibly help.
    ///
    /// Conservative on purpose: a call that is *not* retryable stops the uploader from
    /// hammering an endpoint that will keep saying no, and a wrong `false` here would leave a
    /// file local forever. So anything ambiguous is retryable, and only the four statuses that
    /// mean "you asked wrongly" are not.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Io { .. } | Self::MalformedResponse(_) | Self::RangeMismatch { .. } => true,
            // 403 also covers a clock skew large enough to invalidate the signature, which
            // *is* transient — but retrying it forever hides a wrong secret key, and the log
            // line is how an operator finds out.
            Self::Status { status, .. } => !matches!(status, 400 | 403 | 404 | 405),
            Self::Config(_) => false,
        }
    }

    /// Whether the server said the object is not there.
    ///
    /// The tiered filesystem turns this into `io::ErrorKind::NotFound`, so that a missing
    /// object and a missing local file are the same error to the engine above.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::Status { status: 404, .. })
    }

    pub(crate) fn io(operation: &'static str, source: std::io::Error) -> Self {
        Self::Io { operation, source }
    }
}

/// The result of an S3 call.
pub type Result<T> = std::result::Result<T, Error>;

/// Renders an error body only when there is one, so a bare status reads cleanly.
struct ShowBody<'a>(&'a String);

impl fmt::Display for ShowBody<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.is_empty() {
            Ok(())
        } else {
            write!(f, " — {}", self.0)
        }
    }
}
