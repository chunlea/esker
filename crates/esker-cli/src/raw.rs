//! `esker raw get/put/delete/scan` — the `RawKv` API from a terminal.
//!
//! The thinnest possible shell around `esker-client`: connect, make one call, print the
//! answer. Everything interesting — routing, retries, backoff, the epoch on the header —
//! happens inside `RawClient`, and none of it is re-implemented here.
//!
//! # Printing a value nobody promised was text
//!
//! A value is bytes (`CLAUDE.md` invariant 7). Writing arbitrary bytes to a terminal can
//! reprogram it, so nothing is printed raw unless it is known to be safe: a value that is
//! valid UTF-8 with no control characters prints as itself, and anything else prints as hex,
//! with a line on **stderr** saying so. `stdout` therefore stays exactly the value, which is
//! what a pipe wants; the explanation goes where a pipe does not see it.
//!
//! `--hex` makes that unconditional and symmetric: keys and values on the command line are
//! read as hex, and everything printed is hex. It is the mode for binary keys, which is most
//! real keys once `esker-keys` is encoding them.
//!
//! # Exit codes
//!
//! `0` the call succeeded · `1` the key was not found · `2` the arguments were wrong ·
//! `3` the server refused, or could not be reached.

use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use esker_client::region_cache::StaticRegion;
use esker_client::{RawClient, TcpStores};

use crate::bytes::escape_capped;

/// Where `--addr` points when nothing says otherwise.
///
/// `TiKV`'s store port, deliberately: this is the layer `esker-store` is modelled on, and a
/// familiar number is one less thing to look up.
pub(crate) const DEFAULT_ADDR: &str = "127.0.0.1:20160";

/// The first region of a cluster covers everything and is region 1 (`docs/DESIGN.md` §7).
pub(crate) const BOOTSTRAP_REGION: u64 = 1;

/// `RequestHeader::peer` of zero means "no opinion about the leader", which is the truth for
/// a client that has just connected and been told nothing.
pub(crate) const NO_LEADER_OPINION: u64 = 0;

/// What `esker raw` was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RawCommand {
    /// Read one key.
    Get {
        /// The key.
        key: Vec<u8>,
    },
    /// Write one key.
    Put {
        /// The key.
        key: Vec<u8>,
        /// The value.
        value: Vec<u8>,
    },
    /// Remove one key.
    Delete {
        /// The key.
        key: Vec<u8>,
    },
    /// Read a run of keys.
    Scan {
        /// Inclusive lower bound.
        start: Vec<u8>,
        /// Exclusive upper bound; empty means the end of the key space.
        end: Vec<u8>,
        /// Most pairs to print.
        limit: u32,
        /// Walk from the high end of the range down.
        reverse: bool,
        /// Print keys without their values.
        keys_only: bool,
    },
}

/// How to run one `raw` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RawOptions {
    /// What to do.
    pub(crate) command: RawCommand,
    /// `host:port` of the store.
    pub(crate) addr: String,
    /// Extra stores to connect to, so a `NotLeader` redirect has somewhere to go.
    pub(crate) extra_addrs: Vec<String>,
    /// Read arguments as hex and print results as hex.
    pub(crate) hex: bool,
    /// Wait for a write to be durable before answering.
    pub(crate) sync: bool,
}

impl Default for RawOptions {
    fn default() -> Self {
        Self {
            command: RawCommand::Get { key: Vec::new() },
            addr: DEFAULT_ADDR.to_owned(),
            extra_addrs: Vec::new(),
            hex: false,
            // Durability is an opt-out, never a default (`CLAUDE.md` invariant 1).
            sync: true,
        }
    }
}

/// Whether the command found what it was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The call succeeded.
    Done,
    /// The key is not there. Exit code 1, so a shell script can branch on it.
    NotFound,
}

/// Runs one `raw` command against the store at `options.addr`.
pub(crate) fn run(options: &RawOptions, out: &mut impl Write) -> Result<Outcome, String> {
    // Every `--addr` given, in order. One is phase 2's single store; several are a replicated
    // region, and the extras are what make a redirect usable — a `NotLeader` hint names a peer,
    // the region's peer list turns that into a store, and this book turns *that* into a socket.
    // Until the placement driver exists (phase 4) the operator supplies the book.
    let mut addrs = vec![resolve(&options.addr)?];
    for extra in &options.extra_addrs {
        addrs.push(resolve(extra)?);
    }
    // `ProtoError::NotSent` already names the address it could not reach, so wrapping it in
    // more context would print the address twice.
    let stores = TcpStores::connect_all(&addrs, esker_proto::TransportConfig::new())
        .map_err(|err| err.to_string())?;
    // One address is phase 2's single store, and the region has one peer. Several is a
    // replicated region, and the region has to list them all — otherwise the client learns which
    // peer leads and has no way to reach it.
    let resolver = match stores.only_store() {
        Some(store_id) => {
            StaticRegion::whole_key_space(BOOTSTRAP_REGION, store_id, NO_LEADER_OPINION)
        }
        None => StaticRegion::replicated(BOOTSTRAP_REGION, &stores.store_ids()),
    };
    let client = RawClient::new(Arc::new(stores), Arc::new(resolver));

    match &options.command {
        RawCommand::Get { key } => {
            let Some(value) = client.get(key).map_err(|err| err.to_string())? else {
                return Ok(Outcome::NotFound);
            };
            print_value(out, &value, options.hex)?;
            Ok(Outcome::Done)
        }
        RawCommand::Put { key, value } => {
            client
                .put_with(key, value, options.sync)
                .map_err(|err| err.to_string())?;
            Ok(Outcome::Done)
        }
        RawCommand::Delete { key } => {
            client
                .delete_with(key, options.sync)
                .map_err(|err| err.to_string())?;
            Ok(Outcome::Done)
        }
        RawCommand::Scan {
            start,
            end,
            limit,
            reverse,
            keys_only,
        } => {
            let pairs = if *reverse {
                client.scan_reverse(start, end, *limit)
            } else {
                client.scan(start, end, *limit)
            }
            .map_err(|err| err.to_string())?;

            for (key, value) in &pairs {
                if *keys_only {
                    print_value(out, key, options.hex)?;
                } else {
                    print_pair(out, key, value, options.hex)?;
                }
            }
            // An empty range is a real answer, not a miss: `scan` reports what is there, and
            // nothing being there is something.
            Ok(Outcome::Done)
        }
    }
}

/// Turns `host:port` into an address, preferring IPv4 when a name resolves to both.
pub(crate) fn resolve(addr: &str) -> Result<SocketAddr, String> {
    let mut resolved = addr
        .to_socket_addrs()
        .map_err(|err| format!("{addr}: {err}"))?
        .peekable();
    let mut first = None;
    for candidate in resolved.by_ref() {
        if candidate.is_ipv4() {
            return Ok(candidate);
        }
        first.get_or_insert(candidate);
    }
    first.ok_or_else(|| format!("{addr}: resolved to no address"))
}

/// Whether these bytes can go to a terminal as they are.
///
/// Valid UTF-8 is not enough: a control character can move the cursor, clear the screen or,
/// on some terminals, start something worse. Text means printable text.
fn is_safe_text(bytes: &[u8]) -> bool {
    std::str::from_utf8(bytes).is_ok_and(|text| !text.chars().any(char::is_control))
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Reads a hex string back into bytes. An odd length or a non-hex digit is a usage error.
pub(crate) fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digits = text.as_bytes();
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.chunks_exact(2) {
        let high = (pair[0] as char).to_digit(16)?;
        let low = (pair[1] as char).to_digit(16)?;
        out.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(out)
}

/// Renders one field, and says on stderr when it had to fall back to hex.
fn render(bytes: &[u8], hex: bool) -> String {
    if hex || !is_safe_text(bytes) {
        if !hex {
            eprintln!(
                "esker raw: value is not printable text, shown as hex ({})",
                escape_capped(bytes, 16)
            );
        }
        return to_hex(bytes);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn print_value(out: &mut impl Write, bytes: &[u8], hex: bool) -> Result<(), String> {
    writeln!(out, "{}", render(bytes, hex)).map_err(|err| err.to_string())
}

fn print_pair(out: &mut impl Write, key: &[u8], value: &[u8], hex: bool) -> Result<(), String> {
    writeln!(out, "{}\t{}", render(key, hex), render(value, hex)).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_ADDR, Outcome, RawCommand, RawOptions, from_hex, is_safe_text, render, resolve,
        run, to_hex,
    };
    use crate::testserver::TestServer;

    /// Runs one command against `server` and returns what it printed.
    fn go(server: &TestServer, command: RawCommand) -> (Outcome, String) {
        let options = RawOptions {
            command,
            addr: server.addr(),
            ..RawOptions::default()
        };
        let mut out = Vec::new();
        let outcome = run(&options, &mut out).expect("the call succeeds");
        (
            outcome,
            String::from_utf8(out).expect("printed valid UTF-8"),
        )
    }

    /// The phase's acceptance criterion for this lane: `raw put` then `raw get`, end to end,
    /// over a real socket to a real store.
    #[test]
    fn put_then_get_round_trips_through_a_real_server() {
        let server = TestServer::start();

        let (outcome, printed) = go(
            &server,
            RawCommand::Put {
                key: b"greeting".to_vec(),
                value: b"hello".to_vec(),
            },
        );
        assert_eq!(outcome, Outcome::Done);
        assert!(printed.is_empty(), "a put prints nothing: {printed:?}");

        let (outcome, printed) = go(
            &server,
            RawCommand::Get {
                key: b"greeting".to_vec(),
            },
        );
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(printed, "hello\n");
    }

    /// A key that is not there is exit code 1, not an error and not an empty line — a script
    /// has to be able to tell "absent" from "the cluster is down".
    #[test]
    fn a_missing_key_is_not_found_rather_than_an_error() {
        let server = TestServer::start();
        let (outcome, printed) = go(
            &server,
            RawCommand::Get {
                key: b"never-written".to_vec(),
            },
        );
        assert_eq!(outcome, Outcome::NotFound);
        assert!(printed.is_empty());
    }

    #[test]
    fn delete_removes_a_key_and_an_absent_one_is_not_an_error() {
        let server = TestServer::start();
        go(
            &server,
            RawCommand::Put {
                key: b"k".to_vec(),
                value: b"v".to_vec(),
            },
        );
        assert_eq!(
            go(&server, RawCommand::Delete { key: b"k".to_vec() }).0,
            Outcome::Done
        );
        assert_eq!(
            go(&server, RawCommand::Get { key: b"k".to_vec() }).0,
            Outcome::NotFound
        );
        // Deleting what is not there succeeds; it is a statement about the end state.
        assert_eq!(
            go(&server, RawCommand::Delete { key: b"k".to_vec() }).0,
            Outcome::Done
        );
    }

    #[test]
    fn scan_prints_key_and_value_in_order() {
        let server = TestServer::start();
        for (key, value) in [(&b"a"[..], &b"1"[..]), (b"b", b"2"), (b"c", b"3")] {
            go(
                &server,
                RawCommand::Put {
                    key: key.to_vec(),
                    value: value.to_vec(),
                },
            );
        }

        let scan = |start: &[u8], end: &[u8], limit: u32, keys_only: bool| {
            go(
                &server,
                RawCommand::Scan {
                    start: start.to_vec(),
                    end: end.to_vec(),
                    limit,
                    reverse: false,
                    keys_only,
                },
            )
            .1
        };

        assert_eq!(scan(b"", b"", 10, false), "a\t1\nb\t2\nc\t3\n");
        assert_eq!(scan(b"b", b"", 10, false), "b\t2\nc\t3\n");
        assert_eq!(scan(b"", b"c", 10, false), "a\t1\nb\t2\n");
        assert_eq!(scan(b"", b"", 2, false), "a\t1\nb\t2\n");
        assert_eq!(scan(b"", b"", 10, true), "a\nb\nc\n");
        // An empty range is a real answer, not a miss.
        assert_eq!(scan(b"x", b"", 10, false), "");
    }

    /// A value that is not printable text must not reach the terminal raw, and a `--hex` round
    /// trip has to give back exactly the bytes that went in.
    #[test]
    fn a_binary_value_survives_a_hex_round_trip() {
        let server = TestServer::start();
        let binary = vec![0x00, 0x1b, 0x5b, 0x32, 0x4a, 0xff];
        go(
            &server,
            RawCommand::Put {
                key: b"binary".to_vec(),
                value: binary.clone(),
            },
        );

        let (_, printed) = go(
            &server,
            RawCommand::Get {
                key: b"binary".to_vec(),
            },
        );
        assert_eq!(printed.trim_end(), "001b5b324aff");
        assert_eq!(from_hex(printed.trim_end()).as_deref(), Some(&binary[..]));
    }

    /// The trap this lane was warned about: the `'r'` namespace is the store's, and a client
    /// that added it too would double-prefix. A key that *is* `r` is where that would show.
    #[test]
    fn a_key_that_looks_like_the_namespace_is_stored_as_itself() {
        let server = TestServer::start();
        for key in [&b"r"[..], b"rkey", b"\x00"] {
            go(
                &server,
                RawCommand::Put {
                    key: key.to_vec(),
                    value: b"v".to_vec(),
                },
            );
        }
        let (_, printed) = go(
            &server,
            RawCommand::Scan {
                start: Vec::new(),
                end: Vec::new(),
                limit: 10,
                reverse: false,
                keys_only: true,
            },
        );
        // Three keys, and not one of them grew a prefix on the way through.
        assert_eq!(printed.lines().count(), 3, "{printed:?}");
        assert!(printed.contains("r\n"), "{printed:?}");
        assert!(printed.contains("rkey\n"), "{printed:?}");
    }

    #[test]
    fn hex_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = to_hex(&all);
        assert_eq!(encoded.len(), 512);
        assert_eq!(from_hex(&encoded).as_deref(), Some(&all[..]));
        assert_eq!(to_hex(&[]), "");
        assert_eq!(from_hex("").as_deref(), Some(&[][..]));
    }

    /// Bad hex on the command line is a usage error, not a panic and not a guess.
    #[test]
    fn bad_hex_is_refused() {
        assert_eq!(from_hex("abc"), None, "odd length");
        assert_eq!(from_hex("zz"), None, "not a digit");
        assert_eq!(from_hex("ff ff"), None, "spaces are not hex");
    }

    /// Valid UTF-8 is not the test — a control character is valid UTF-8 and can reprogram a
    /// terminal. Printable text is the test.
    #[test]
    fn only_printable_text_prints_as_itself() {
        assert!(is_safe_text(b"hello"));
        assert!(
            is_safe_text("caf\u{e9}".as_bytes()),
            "multi-byte UTF-8 is text"
        );
        assert!(is_safe_text(b""));
        assert!(!is_safe_text(b"\xff\xfe"), "not UTF-8");
        assert!(!is_safe_text(b"line\nbreak"), "a newline would split a row");
        assert!(
            !is_safe_text(b"\x1b[2J"),
            "an escape sequence must never go raw"
        );
        assert!(!is_safe_text(b"nul\0"));
    }

    #[test]
    fn rendering_falls_back_to_hex_and_hex_mode_is_unconditional() {
        assert_eq!(render(b"plain", false), "plain");
        assert_eq!(render(b"\x1b[2J", false), "1b5b324a");
        assert_eq!(render(b"plain", true), "706c61696e");
    }

    #[test]
    fn the_default_address_parses() {
        let addr = resolve(DEFAULT_ADDR).expect("the default must be usable");
        assert_eq!(addr.port(), 20160);
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn an_unusable_address_is_an_error_rather_than_a_panic() {
        assert!(resolve("not a host:port").is_err());
        assert!(resolve("127.0.0.1").is_err(), "a port is required");
    }
}
