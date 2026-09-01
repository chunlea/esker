//! The key space: a memcomparable codec whose encoded byte order equals logical order, the
//! reserved prefixes that divide the cluster's keys between `RawKV`, transactions, SQL and
//! metadata, and the MVCC timestamp suffix. Everything below this crate treats keys as opaque
//! bytes ordered by `memcmp`; this is where they get meaning (`docs/DESIGN.md` §3).
//!
//! # Invariants
//!
//! * **Encoded order is logical order.** For every type, `a < b` if and only if
//!   `encode(a) < encode(b)` bytewise. Range scans in the engine depend on it.
//! * **The bytes encoding is prefix-free.** No encoded value is a byte prefix of another, so
//!   concatenating fields in a tuple cannot make two different tuples compare equal.
//! * **Newer versions sort first.** `enc_ts` is the bitwise complement of the timestamp, so a
//!   point read is "the first key under this prefix" rather than a scan to the end.
//! * **Decoding never panics.** Malformed bytes come back as an error (`CLAUDE.md`
//!   invariant 9), and the encoding is canonical: exactly one byte string per value.
//! * **Key semantics stop here.** `esker-engine` and `esker-raft` must never depend on this
//!   crate (invariant 7).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod codec;
pub mod columnar;
pub mod prefix;
pub mod row;
pub mod value;

pub use codec::{
    CodecError, Value, ValueKind, dec_ts, decode_bytes, decode_i64, decode_tuple, decode_u64,
    enc_ts, encode_bytes, encode_i64, encode_tuple, encode_u64,
};
