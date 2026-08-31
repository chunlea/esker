//! AWS Signature Version 4.
//!
//! Signing is the part of an S3 client that cannot be approximately right. Every field of the
//! canonical request is hashed, so a stray space, a header sorted by the wrong key, or a path
//! encoded once too often produces a signature that differs from the server's in every byte,
//! and the server's only reply is `403 SignatureDoesNotMatch` — which says nothing about which
//! of the twenty things it was.
//!
//! The shape, from AWS's "Signature Version 4 signing process":
//!
//! ```text
//! canonical request = METHOD ++ URI ++ QUERY ++ HEADERS ++ SIGNED-HEADERS ++ hex(H(payload))
//! string to sign    = "AWS4-HMAC-SHA256" ++ timestamp ++ scope ++ hex(H(canonical request))
//! signing key       = HMAC(HMAC(HMAC(HMAC("AWS4"++secret, date), region), service), "aws4_request")
//! signature         = hex(HMAC(signing key, string to sign))
//! ```
//!
//! Each stage is a separate function with its own test, because a bug in one of them is
//! invisible in the composite: the only observable is the final hex string, and every stage
//! can produce a plausible-looking one.
//!
//! # The S3 exception
//!
//! Most services canonicalise the path by URI-encoding it **twice**. S3 does not — the
//! resource path is encoded once and `/` is left alone. This is called out in AWS's own
//! documentation as an exception, and it is the single most common way a hand-written signer
//! works against every service except the one it was written for. [`CanonicalRequest`] encodes
//! once, and `a_path_is_encoded_exactly_once` in the tests below pins that.

use esker_base::hmac::hmac_sha256;
use esker_base::sha256::{self, DIGEST_LEN};

/// The only algorithm we sign with.
pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";

/// The terminator every credential scope ends with.
const TERMINATOR: &str = "aws4_request";

/// The long-lived half of an S3 identity.
///
/// Cloned rather than borrowed because the uploader outlives every request it makes, and a
/// secret that is a `&str` somewhere has to be a `String` somewhere else anyway.
#[derive(Clone)]
pub struct Credentials {
    /// The public half, which appears in the `Authorization` header.
    pub access_key_id: String,
    /// The secret half, which never appears anywhere — only HMACs of it do.
    pub secret_access_key: String,
    /// A session token, for temporary credentials. Sent as `x-amz-security-token`.
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    /// Redacted by construction. A `Debug` that prints a secret access key is how a secret ends
    /// up in a log file, and every type in this crate derives `Debug` for `tracing`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl Credentials {
    /// Credentials from a key pair.
    pub fn new(access_key_id: impl Into<String>, secret_access_key: impl Into<String>) -> Self {
        Self {
            access_key_id: access_key_id.into(),
            secret_access_key: secret_access_key.into(),
            session_token: None,
        }
    }

    /// The same, with a session token attached.
    #[must_use]
    pub fn with_session_token(mut self, token: impl Into<String>) -> Self {
        self.session_token = Some(token.into());
        self
    }
}

/// Everything about *this* signature that is not the request itself.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    /// `YYYYMMDD`, which must be the date part of [`Self::timestamp`].
    pub date: &'a str,
    /// `YYYYMMDDTHHMMSSZ`, the value of the `x-amz-date` header.
    pub timestamp: &'a str,
    /// The region the bucket lives in. `MinIO` accepts anything but must be told the same thing
    /// twice, so this is configuration rather than a constant.
    pub region: &'a str,
    /// Always `"s3"` here, but the algorithm does not care and the tests use others.
    pub service: &'a str,
}

impl Scope<'_> {
    /// `YYYYMMDD/region/service/aws4_request`.
    #[must_use]
    pub fn credential_scope(&self) -> String {
        format!(
            "{}/{}/{}/{TERMINATOR}",
            self.date, self.region, self.service
        )
    }
}

/// The canonical form of a request, as the string that gets hashed.
///
/// Built rather than formatted in one shot so that each stage can be inspected — the tests
/// compare against AWS's published canonical requests directly, which is the only way to find
/// out *which* field is wrong when a signature does not match.
#[derive(Debug, Clone)]
pub struct CanonicalRequest {
    /// The rendered canonical request, newline-separated.
    pub text: String,
    /// `host;x-amz-content-sha256;x-amz-date`, the semicolon-joined signed header names.
    pub signed_headers: String,
}

impl CanonicalRequest {
    /// Builds the canonical request.
    ///
    /// `path` is the raw, *unencoded* object path (`/bucket/prefix/000007.sst`); it is encoded
    /// here, once, preserving `/`. `query` need not be sorted — this sorts it. `headers` need
    /// not be lowercase or sorted — this lowercases and sorts them, and signs every one of
    /// them, because a header we send and do not sign is a header a middlebox may change
    /// without the server noticing.
    pub fn new(
        method: &str,
        path: &str,
        query: &[(String, String)],
        headers: &[(String, String)],
        payload_sha256_hex: &str,
    ) -> Self {
        let canonical_uri = encode_path(path);

        // `true`: a query parameter encodes `/` as `%2F`. Only the *path* leaves it alone, and
        // a continuation token is base64, so this is the difference between a listing that
        // paginates and one that returns 403 on its second page.
        let mut params: Vec<(String, String)> = query
            .iter()
            .map(|(name, value)| (uri_encode(name, true), uri_encode(value, true)))
            .collect();
        // Sorted by encoded name, then by encoded value: AWS specifies byte order over the
        // encoded forms, and `%2F` sorts differently from `/`.
        params.sort();
        let canonical_query = params
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");

        let mut named: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), trim_header_value(value)))
            .collect();
        named.sort();

        let mut canonical_headers = String::new();
        for (name, value) in &named {
            canonical_headers.push_str(name);
            canonical_headers.push(':');
            canonical_headers.push_str(value);
            canonical_headers.push('\n');
        }
        let signed_headers = named
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");

        let text = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_sha256_hex}"
        );
        Self {
            text,
            signed_headers,
        }
    }

    /// `hex(SHA-256(canonical request))`, the last line of the string to sign.
    #[must_use]
    pub fn hash_hex(&self) -> String {
        sha256::hex(&sha256::digest(self.text.as_bytes()))
    }
}

/// The string that actually gets HMAC'd.
#[must_use]
pub fn string_to_sign(scope: &Scope<'_>, canonical: &CanonicalRequest) -> String {
    format!(
        "{ALGORITHM}\n{}\n{}\n{}",
        scope.timestamp,
        scope.credential_scope(),
        canonical.hash_hex()
    )
}

/// The four-stage key derivation.
///
/// Each stage narrows the key: a key derived for one date, region and service cannot sign for
/// another, which is what makes a leaked *signing* key far less interesting than a leaked
/// secret. We do not cache it — one derivation is four HMACs over short strings, which is
/// nothing next to hashing an 8 MiB SST body.
#[must_use]
pub fn signing_key(secret_access_key: &str, scope: &Scope<'_>) -> [u8; DIGEST_LEN] {
    let mut key = Vec::with_capacity(4 + secret_access_key.len());
    key.extend_from_slice(b"AWS4");
    key.extend_from_slice(secret_access_key.as_bytes());

    let date = hmac_sha256(&key, scope.date.as_bytes());
    let region = hmac_sha256(&date, scope.region.as_bytes());
    let service = hmac_sha256(&region, scope.service.as_bytes());
    hmac_sha256(&service, TERMINATOR.as_bytes())
}

/// The hex signature for one request.
#[must_use]
pub fn signature(secret_access_key: &str, scope: &Scope<'_>, to_sign: &str) -> String {
    let key = signing_key(secret_access_key, scope);
    sha256::hex(&hmac_sha256(&key, to_sign.as_bytes()))
}

/// The complete `Authorization` header value.
#[must_use]
pub fn authorization_header(
    credentials: &Credentials,
    scope: &Scope<'_>,
    canonical: &CanonicalRequest,
) -> String {
    let to_sign = string_to_sign(scope, canonical);
    let signature = signature(&credentials.secret_access_key, scope, &to_sign);
    format!(
        "{ALGORITHM} Credential={}/{}, SignedHeaders={}, Signature={signature}",
        credentials.access_key_id,
        scope.credential_scope(),
        canonical.signed_headers
    )
}

/// RFC 3986 percent-encoding, AWS flavour.
///
/// Unreserved characters (`A-Z a-z 0-9 - _ . ~`) pass through; everything else becomes `%XX`
/// with **uppercase** hex, because the canonical request is compared byte for byte and
/// lowercase would differ. `encode_slash` is false for a path, where `/` separates segments and
/// must survive, and true everywhere else.
#[must_use]
pub fn uri_encode(value: &str, encode_slash: bool) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(*byte));
            }
            b'/' if !encode_slash => out.push('/'),
            other => {
                out.push('%');
                out.push(char::from(DIGITS[usize::from(other >> 4)]));
                out.push(char::from(DIGITS[usize::from(other & 0x0f)]));
            }
        }
    }
    out
}

/// The canonical URI: the path encoded exactly **once**, `/` preserved.
///
/// See the module docs — most AWS services encode the path twice here and S3 is the documented
/// exception. An empty path canonicalises to `/`, which is what a request to the service root
/// signs.
fn encode_path(path: &str) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    uri_encode(path, false)
}

/// A header value as the canonical request wants it: outer whitespace gone, inner runs of
/// spaces collapsed to one.
///
/// The collapsing rule exists because HTTP lets a sender pad a value and a proxy re-pad it, so
/// the signature must not depend on padding. It applies outside quoted strings only; none of
/// the headers this crate sends contain quotes, and a value that did would be signed
/// conservatively rather than wrongly.
fn trim_header_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_space = false;
    for ch in value.trim().chars() {
        if ch == ' ' || ch == '\t' {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(ch);
    }
    out
}

/// `YYYYMMDDTHHMMSSZ` for a Unix timestamp, and its `YYYYMMDD` prefix.
///
/// Written out rather than taken from a date crate: this is the only calendar arithmetic in the
/// project and it is twenty lines. The civil-from-days conversion is the standard one (Howard
/// Hinnant's `civil_from_days`), which is exact for every day in the proleptic Gregorian
/// calendar and has no leap-second concept — neither does Unix time, and neither does AWS.
#[must_use]
pub fn format_amz_date(unix_seconds: i64) -> (String, String) {
    let days = unix_seconds.div_euclid(86_400);
    let secs_of_day = unix_seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );
    let date = format!("{year:04}{month:02}{day:02}");
    let timestamp = format!("{date}T{hour:02}{minute:02}{second:02}Z");
    (date, timestamp)
}

/// Days since the Unix epoch to a `(year, month, day)` in the proleptic Gregorian calendar.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the year and makes
    // the whole thing branch-free.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11], March = 0
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (year + i64::from(month <= 2), month, day)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        CanonicalRequest, Credentials, Scope, authorization_header, civil_from_days,
        format_amz_date, signature, signing_key, string_to_sign, trim_header_value, uri_encode,
    };
    use esker_base::sha256::{self, EMPTY_DIGEST_HEX};

    /// The credentials every AWS `SigV4` example uses. Published, expired, and famous.
    const ACCESS_KEY: &str = "AKIDEXAMPLE";
    const SECRET_KEY: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn header(name: &str, value: &str) -> (String, String) {
        (name.to_string(), value.to_string())
    }

    /// `get-vanilla` from AWS's `aws-sig-v4-test-suite`: the simplest possible signed request,
    /// and therefore the one that isolates the framework from the fields.
    ///
    /// Provenance: the `aws-sig-v4-test-suite` archive AWS publishes alongside the "Signature
    /// Version 4 signing process" documentation — directory `get-vanilla`, files `.creq`,
    /// `.sts` and `.authz`. Region `us-east-1`, service `service`, timestamp
    /// `20150830T123600Z`, the credentials above. The three expected strings below are those
    /// files verbatim.
    #[test]
    fn get_vanilla_from_the_aws_test_suite() {
        let scope = Scope {
            date: "20150830",
            timestamp: "20150830T123600Z",
            region: "us-east-1",
            service: "service",
        };
        let canonical = CanonicalRequest::new(
            "GET",
            "/",
            &[],
            &[
                header("Host", "example.amazonaws.com"),
                header("X-Amz-Date", "20150830T123600Z"),
            ],
            EMPTY_DIGEST_HEX,
        );

        assert_eq!(
            canonical.text,
            "GET\n\
             /\n\
             \n\
             host:example.amazonaws.com\n\
             x-amz-date:20150830T123600Z\n\
             \n\
             host;x-amz-date\n\
             e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            string_to_sign(&scope, &canonical),
            "AWS4-HMAC-SHA256\n\
             20150830T123600Z\n\
             20150830/us-east-1/service/aws4_request\n\
             bb579772317eb040ac9ed261061d46c1f17a8133879d6129b6e1c25292927e63"
        );

        let credentials = Credentials::new(ACCESS_KEY, SECRET_KEY);
        assert_eq!(
            authorization_header(&credentials, &scope, &canonical),
            "AWS4-HMAC-SHA256 \
             Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// The worked example in AWS's own signing documentation — an IAM `ListUsers` call — which
    /// publishes the **intermediate** signing key as well as the signature. That makes it the
    /// only vector that can tell a broken key derivation apart from a broken canonical request:
    /// every other test fails identically for both.
    ///
    /// Provenance: "Signature Version 4 signing process", the worked
    /// `GET https://iam.amazonaws.com/?Action=ListUsers&Version=2010-05-08` example, tasks 1
    /// through 4.
    #[test]
    fn the_iam_worked_example_including_its_signing_key() {
        let scope = Scope {
            date: "20150830",
            timestamp: "20150830T123600Z",
            region: "us-east-1",
            service: "iam",
        };

        assert_eq!(
            sha256::hex(&signing_key(SECRET_KEY, &scope)),
            "c4afb1cc5771d871763a393e44b703571b55cc28424d1a5e86da6ed3c154a4b9",
            "the four-stage key derivation"
        );

        let canonical = CanonicalRequest::new(
            "GET",
            "/",
            &[
                ("Action".to_string(), "ListUsers".to_string()),
                ("Version".to_string(), "2010-05-08".to_string()),
            ],
            &[
                header(
                    "Content-Type",
                    "application/x-www-form-urlencoded; charset=utf-8",
                ),
                header("Host", "iam.amazonaws.com"),
                header("X-Amz-Date", "20150830T123600Z"),
            ],
            EMPTY_DIGEST_HEX,
        );
        assert_eq!(
            canonical.hash_hex(),
            "f536975d06c0309214f805bb90ccff089219ecd68b2577efef23edd43b7e1a59"
        );
        assert_eq!(
            signature(SECRET_KEY, &scope, &string_to_sign(&scope, &canonical)),
            "5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
        );
    }

    /// The S3 exception, pinned. A path is encoded once: `/` survives and a space becomes
    /// `%20`, not `%2520`. Signing an SST key with a double encoder produces a valid-looking
    /// signature that S3 rejects and `MinIO` rejects differently.
    #[test]
    fn a_path_is_encoded_exactly_once() {
        let canonical = CanonicalRequest::new(
            "PUT",
            "/esker/sst tier/000007.sst",
            &[],
            &[header("Host", "localhost:9000")],
            EMPTY_DIGEST_HEX,
        );
        let uri = canonical.text.lines().nth(1).unwrap();
        assert_eq!(uri, "/esker/sst%20tier/000007.sst");
        assert!(!uri.contains("%25"), "the path was encoded twice: {uri}");
    }

    /// Query parameters sort by their **encoded** name, and a valueless parameter still gets
    /// its `=`. `ListObjectsV2` sends `list-type=2&prefix=...&continuation-token=...`, and a
    /// continuation token is base64 — full of `+`, `/` and `=`, every one of which must be
    /// escaped.
    #[test]
    fn the_query_string_is_sorted_and_encoded() {
        let canonical = CanonicalRequest::new(
            "GET",
            "/esker",
            &[
                ("prefix".to_string(), "tier/00".to_string()),
                ("list-type".to_string(), "2".to_string()),
                ("continuation-token".to_string(), "a+b/c=".to_string()),
                ("fetch-owner".to_string(), String::new()),
            ],
            &[header("Host", "s3.amazonaws.com")],
            EMPTY_DIGEST_HEX,
        );
        assert_eq!(
            canonical.text.lines().nth(2).unwrap(),
            "continuation-token=a%2Bb%2Fc%3D&fetch-owner=&list-type=2&prefix=tier%2F00"
        );
    }

    /// Header names are lowercased and sorted, values are trimmed and their inner runs of
    /// spaces collapsed. This is `get-header-value-trim` from the same suite, in spirit: the
    /// signature must not depend on padding a proxy is free to change.
    #[test]
    fn headers_are_lowercased_sorted_and_trimmed() {
        let canonical = CanonicalRequest::new(
            "PUT",
            "/b/k",
            &[],
            &[
                header("X-Amz-Date", "20150830T123600Z"),
                header("Host", "example.amazonaws.com"),
                header("My-Header", "  a  b   c  "),
            ],
            EMPTY_DIGEST_HEX,
        );
        let lines: Vec<&str> = canonical.text.lines().collect();
        assert_eq!(lines[3], "host:example.amazonaws.com");
        assert_eq!(lines[4], "my-header:a b c");
        assert_eq!(lines[5], "x-amz-date:20150830T123600Z");
        assert_eq!(canonical.signed_headers, "host;my-header;x-amz-date");
    }

    #[test]
    fn trimming_handles_the_degenerate_values() {
        assert_eq!(trim_header_value(""), "");
        assert_eq!(trim_header_value("   "), "");
        assert_eq!(trim_header_value("\ta\t"), "a");
        assert_eq!(trim_header_value("a"), "a");
    }

    /// Uppercase hex, and `~` left alone — two details AWS specifies and most percent-encoders
    /// get wrong in opposite directions.
    #[test]
    fn uri_encoding_is_uppercase_and_leaves_tilde_alone() {
        assert_eq!(uri_encode("a~z-_.", true), "a~z-_.");
        assert_eq!(uri_encode(" ", true), "%20");
        assert_eq!(uri_encode("/", true), "%2F");
        assert_eq!(uri_encode("/", false), "/");
        assert_eq!(uri_encode("é", true), "%C3%A9");
        assert_eq!(uri_encode("+", true), "%2B");
    }

    /// The signature depends on the secret, the date, the region and the service. Changing any
    /// one of them must change it — a derivation that dropped a stage would still produce a
    /// stable, wrong answer.
    #[test]
    fn every_stage_of_the_key_derivation_matters() {
        let base = Scope {
            date: "20150830",
            timestamp: "20150830T123600Z",
            region: "us-east-1",
            service: "s3",
        };
        let reference = signing_key(SECRET_KEY, &base);
        for changed in [
            Scope {
                date: "20150831",
                ..base
            },
            Scope {
                region: "eu-west-1",
                ..base
            },
            Scope {
                service: "iam",
                ..base
            },
        ] {
            assert_ne!(signing_key(SECRET_KEY, &changed), reference);
        }
        assert_ne!(signing_key("another-secret", &base), reference);
    }

    /// The calendar. `1970-01-01` is day zero by definition; the rest are the cases that break
    /// a hand-written conversion — a leap day, the year-2000 leap that the century rule
    /// exempts from its own exemption, and a date after 2038.
    #[test]
    fn the_calendar_is_right_where_calendars_go_wrong() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(
            format_amz_date(0),
            ("19700101".into(), "19700101T000000Z".into())
        );
        // 2015-08-30T12:36:00Z, the timestamp every vector above uses.
        assert_eq!(
            format_amz_date(1_440_938_160).1,
            "20150830T123600Z",
            "the AWS example timestamp must round-trip"
        );
        // 2000-02-29: a leap year because 400 divides it, despite 100 dividing it.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        // 2024-02-29, an ordinary leap day.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        // 2038-01-19T03:14:07Z, the last second of a 32-bit time_t.
        assert_eq!(format_amz_date(2_147_483_647).1, "20380119T031407Z");
        // A time before the epoch must not panic or produce a negative field.
        assert_eq!(format_amz_date(-1).1, "19691231T235959Z");
    }

    /// The date in the credential scope must be the date part of the timestamp. Nothing
    /// enforces that structurally, so the constructor that builds both is where it is checked
    /// — here, over a day's worth of seconds around a midnight.
    #[test]
    fn the_scope_date_is_the_timestamp_prefix() {
        for offset in [-2i64, -1, 0, 1, 2, 86_399, 86_400] {
            let (date, timestamp) = format_amz_date(1_440_938_160 + offset);
            assert_eq!(&timestamp[..8], date, "at offset {offset}");
        }
    }
}
