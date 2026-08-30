//! The `TxnKv` service — service `0x02`, reserved since phase 2 and filled in here
//! (`docs/DESIGN.md` §9, `docs/txn-spec.md`).
//!
//! Eight methods, in the order `docs/DESIGN.md` §9 lists them:
//!
//! ```text
//! 0x0201 Get           read one key at a timestamp
//! 0x0202 Scan          read a range at a timestamp
//! 0x0203 Prewrite      lock and stage a batch of keys for one transaction
//! 0x0204 Commit        write the commit records for keys already prewritten
//! 0x0205 Rollback      abandon a transaction's keys, leaving markers
//! 0x0206 ResolveLock   finish someone else's transaction, either way
//! 0x0207 Heartbeat     extend a live transaction's lock TTL
//! 0x0208 GcSafepoint   publish the timestamp below which old versions may go
//! ```
//!
//! # What is here and what is not
//!
//! This module is the **wire**: the request and response bodies and their framing. The
//! *records* — `LockRecord`, `WriteRecord`, the `'x'`-space keys — are `esker-txn`'s and stay
//! there; nothing in `esker-proto` decodes one. The one Percolator shape that has to be here
//! is [`LockInfo`], because it travels inside `ProtoError::Locked` and every peer that speaks
//! the protocol has to be able to read it.
//!
//! [`LockInfo`] is deliberately **not** `esker_txn::LockRecord`. They describe the same lock
//! and share no fields by accident: the stored record has a `kind` and an inline value, which
//! are nobody's business on the wire, and the wire form has the *key*, which the stored record
//! does not need because it is the key it is filed under. A caller resolving a lock needs
//! exactly the four fields here.
//!
//! # Keys are user keys
//!
//! Every key in every message is the raw user key. The `'x'` namespace and the version suffix
//! are applied by the store on the way to the engine, on the same rule that keeps `'r'` out of
//! `RawKv` requests (`docs/DESIGN.md` §10).

use bytes::Bytes;

use crate::codec::{DecodeError, Decoder, Encoder};
use crate::error::ProtoError;
use crate::messages::Method;

/// One lock, as a reader or a writer that collided with it sees it.
///
/// Carried in [`ProtoError::Locked`]'s opaque payload — opaque so that the error enum does not
/// change shape every time a lock gains a field, and because `esker-proto`'s error golden is
/// frozen. [`encode`](LockInfo::encode) and [`decode`](LockInfo::decode) are the only readers
/// of those bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockInfo {
    /// The user key that is locked.
    pub key: Bytes,
    /// The user key of the locking transaction's **primary** — the one whose `write` record
    /// decides whether the transaction committed. This is what makes the lock resolvable.
    pub primary: Bytes,
    /// The locking transaction's snapshot.
    pub start_ts: u64,
    /// How long the lock lives without a heartbeat, in milliseconds from `start_ts`'s
    /// physical part.
    pub ttl_ms: u64,
}

impl LockInfo {
    /// The lock's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Encoder::with_capacity(32 + self.key.len() + self.primary.len());
        out.put_bytes(&self.key);
        out.put_bytes(&self.primary);
        out.put_varint(self.start_ts);
        out.put_varint(self.ttl_ms);
        out.finish()
    }

    /// Reads a lock. Trailing bytes are an error, as everywhere else in this crate.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut input = Decoder::new(bytes);
        let lock = Self {
            key: take(&mut input, "lock.key")?,
            primary: take(&mut input, "lock.primary")?,
            start_ts: input.get_varint("lock.start_ts")?,
            ttl_ms: input.get_varint("lock.ttl_ms")?,
        };
        input.finish()?;
        Ok(lock)
    }

    /// The typed refusal a store answers a locked key with.
    #[must_use]
    pub fn into_error(&self) -> ProtoError {
        ProtoError::Locked {
            lock_info: Bytes::from(self.encode()),
        }
    }

    /// The lock inside a [`ProtoError::Locked`], or `None` for any other error.
    ///
    /// Answers `Err` for a `Locked` whose payload will not decode: a refusal a client cannot
    /// read is not a refusal it may treat as "no lock in the way".
    pub fn from_error(error: &ProtoError) -> Option<Result<Self, DecodeError>> {
        match error {
            ProtoError::Locked { lock_info } => Some(Self::decode(lock_info)),
            _ => None,
        }
    }
}

/// One key's worth of a [`TxnKvReq::Prewrite`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnMutation {
    /// Write `value` at `key`.
    Put {
        /// The user key.
        key: Bytes,
        /// The value.
        value: Bytes,
    },
    /// Remove `key`.
    Delete {
        /// The user key.
        key: Bytes,
    },
}

impl TxnMutation {
    /// The user key this touches.
    #[must_use]
    pub fn key(&self) -> &Bytes {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }

    /// The tag on the wire. Zero is not a tag, as everywhere else
    /// (`docs/DESIGN.md` §4.3, §9).
    fn tag(&self) -> u8 {
        match self {
            Self::Put { .. } => 1,
            Self::Delete { .. } => 2,
        }
    }

    fn encode(&self, out: &mut Encoder) {
        out.put_u8(self.tag());
        match self {
            Self::Put { key, value } => {
                out.put_bytes(key);
                out.put_bytes(value);
            }
            Self::Delete { key } => out.put_bytes(key),
        }
    }

    fn decode(input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        match input.get_u8("mutation.tag")? {
            1 => Ok(Self::Put {
                key: take(input, "mutation.key")?,
                value: take(input, "mutation.value")?,
            }),
            2 => Ok(Self::Delete {
                key: take(input, "mutation.key")?,
            }),
            tag => Err(DecodeError::invalid(
                "mutation.tag",
                format!("{tag} is not a mutation kind"),
            )),
        }
    }
}

/// Anything a client asks the `TxnKv` service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnKvReq {
    /// Read one key as of `ts`.
    Get {
        /// The user key.
        key: Bytes,
        /// The reading transaction's snapshot.
        ts: u64,
    },
    /// Read `[start, end)` as of `ts`, at most `limit` pairs.
    Scan {
        /// Inclusive lower bound.
        start: Bytes,
        /// Exclusive upper bound; empty means the end of the key space.
        end: Bytes,
        /// Most pairs to return.
        limit: u32,
        /// The reading transaction's snapshot.
        ts: u64,
        /// Walk from the high end of the range towards the low one.
        reverse: bool,
    },
    /// Lock and stage a batch of one transaction's keys.
    ///
    /// Every key in the batch is in one region — splitting a transaction across regions is the
    /// client's job — and the whole batch is one decision: if any key conflicts, none is
    /// written.
    Prewrite {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The user key of the transaction's primary. Present on the primary's own batch too,
        /// where it equals that key.
        primary: Bytes,
        /// How long the locks should live without a heartbeat.
        ttl_ms: u64,
        /// What to write.
        mutations: Vec<TxnMutation>,
    },
    /// Write the commit records for keys this transaction has already prewritten.
    Commit {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The timestamp every key of the transaction commits at.
        commit_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Abandon this transaction's keys, leaving a rollback marker on each.
    Rollback {
        /// The transaction's snapshot.
        start_ts: u64,
        /// The user keys.
        keys: Vec<Bytes>,
    },
    /// Finish *another* transaction's keys, the way its primary says.
    ///
    /// The caller has already read the primary's state; this is the write half of
    /// `docs/txn-spec.md` §5.5. `commit_ts` of zero means roll back, which is unambiguous
    /// because no transaction commits at timestamp zero.
    ResolveLock {
        /// The stuck transaction's snapshot.
        start_ts: u64,
        /// Its commit timestamp, or zero to roll it back.
        commit_ts: u64,
        /// The user keys to finish. Empty means "every key of that transaction in this
        /// region", which is what a reader that met one lock asks for.
        keys: Vec<Bytes>,
    },
    /// Extend a live transaction's lock TTL.
    Heartbeat {
        /// The transaction's snapshot.
        start_ts: u64,
        /// Its primary — the only lock that is heartbeated, because it is the only one a
        /// resolver consults (`docs/txn-spec.md` §5.5).
        primary: Bytes,
        /// The TTL to extend to, in milliseconds from `start_ts`'s physical part. A store
        /// never shortens a lock, so a lower value than the lock already has is ignored.
        ttl_ms: u64,
    },
    /// Publish the timestamp below which old MVCC versions may be collected.
    GcSafepoint {
        /// The new safepoint. A store never moves its safepoint backwards.
        safepoint: u64,
    },
}

impl TxnKvReq {
    /// The method this request is sent as.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Get { .. } => Method::TxnGet,
            Self::Scan { .. } => Method::TxnScan,
            Self::Prewrite { .. } => Method::TxnPrewrite,
            Self::Commit { .. } => Method::TxnCommit,
            Self::Rollback { .. } => Method::TxnRollback,
            Self::ResolveLock { .. } => Method::TxnResolveLock,
            Self::Heartbeat { .. } => Method::TxnHeartbeat,
            Self::GcSafepoint { .. } => Method::TxnGcSafepoint,
        }
    }

    /// The key this request routes by: the region cache is consulted with it.
    ///
    /// For the batch methods that is the first key, and for a range it is the lower bound —
    /// splitting a batch across regions is the client's job, before the request is built. A
    /// request with no keys routes to the start of the key space rather than panicking
    /// (`CLAUDE.md` invariant 9).
    #[must_use]
    pub fn routing_key(&self) -> &[u8] {
        match self {
            Self::Get { key, .. } => key,
            Self::Scan { start, .. } => start,
            Self::Prewrite { mutations, .. } => mutations.first().map_or(&[][..], |m| m.key()),
            Self::Commit { keys, .. }
            | Self::Rollback { keys, .. }
            | Self::ResolveLock { keys, .. } => keys.first().map_or(&[][..], |key| &key[..]),
            Self::Heartbeat { primary, .. } => primary,
            Self::GcSafepoint { .. } => &[],
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Get { key, ts } => {
                out.put_bytes(key);
                out.put_varint(*ts);
            }
            Self::Scan {
                start,
                end,
                limit,
                ts,
                reverse,
            } => {
                out.put_bytes(start);
                out.put_bytes(end);
                out.put_varint(u64::from(*limit));
                out.put_varint(*ts);
                out.put_bool(*reverse);
            }
            Self::Prewrite {
                start_ts,
                primary,
                ttl_ms,
                mutations,
            } => {
                out.put_varint(*start_ts);
                out.put_bytes(primary);
                out.put_varint(*ttl_ms);
                out.put_varint(mutations.len() as u64);
                for mutation in mutations {
                    mutation.encode(out);
                }
            }
            // `Commit` and `ResolveLock` have the same three fields and encode the same
            // way. They stay two methods because they are two different acts — finishing
            // one's own transaction, and finishing someone else's — and a store logs,
            // authorises and counts them separately.
            Self::Commit {
                start_ts,
                commit_ts,
                keys,
            }
            | Self::ResolveLock {
                start_ts,
                commit_ts,
                keys,
            } => {
                out.put_varint(*start_ts);
                out.put_varint(*commit_ts);
                put_keys(out, keys);
            }
            Self::Rollback { start_ts, keys } => {
                out.put_varint(*start_ts);
                put_keys(out, keys);
            }
            Self::Heartbeat {
                start_ts,
                primary,
                ttl_ms,
            } => {
                out.put_varint(*start_ts);
                out.put_bytes(primary);
                out.put_varint(*ttl_ms);
            }
            Self::GcSafepoint { safepoint } => out.put_varint(*safepoint),
        }
    }

    pub(crate) fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let request = match method {
            Method::TxnGet => Self::Get {
                key: take(input, "key")?,
                ts: input.get_varint("ts")?,
            },
            Method::TxnScan => Self::Scan {
                start: take(input, "start")?,
                end: take(input, "end")?,
                limit: input.get_varint_u32("limit")?,
                ts: input.get_varint("ts")?,
                reverse: input.get_bool("reverse")?,
            },
            Method::TxnPrewrite => {
                let start_ts = input.get_varint("start_ts")?;
                let primary = take(input, "primary")?;
                let ttl_ms = input.get_varint("ttl_ms")?;
                let count = input.get_count("mutations")?;
                let mut mutations = Vec::with_capacity(count);
                for _ in 0..count {
                    mutations.push(TxnMutation::decode(input)?);
                }
                Self::Prewrite {
                    start_ts,
                    primary,
                    ttl_ms,
                    mutations,
                }
            }
            Method::TxnCommit => Self::Commit {
                start_ts: input.get_varint("start_ts")?,
                commit_ts: input.get_varint("commit_ts")?,
                keys: get_keys(input)?,
            },
            Method::TxnRollback => Self::Rollback {
                start_ts: input.get_varint("start_ts")?,
                keys: get_keys(input)?,
            },
            Method::TxnResolveLock => Self::ResolveLock {
                start_ts: input.get_varint("start_ts")?,
                commit_ts: input.get_varint("commit_ts")?,
                keys: get_keys(input)?,
            },
            Method::TxnHeartbeat => Self::Heartbeat {
                start_ts: input.get_varint("start_ts")?,
                primary: take(input, "primary")?,
                ttl_ms: input.get_varint("ttl_ms")?,
            },
            Method::TxnGcSafepoint => Self::GcSafepoint {
                safepoint: input.get_varint("safepoint")?,
            },
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a TxnKv method", other.name()),
                ));
            }
        };
        Ok(request)
    }
}

/// The answer to a [`TxnKvReq`]. One variant per method, and the method is echoed in the
/// frame's tag, so a demultiplexer never has to guess what it is holding.
///
/// A lock in the way is **not** here: it is a `ProtoError::Locked` carrying a [`LockInfo`],
/// because it is a refusal to serve rather than an answer, and because the client's retry
/// machinery already classifies errors in one place (`docs/DESIGN.md` §10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnKvResp {
    /// The value at the read timestamp, or `None` if the key does not exist there.
    Get {
        /// The value.
        value: Option<Bytes>,
    },
    /// The pairs in the range, in key order.
    Scan {
        /// Key and value.
        pairs: Vec<(Bytes, Bytes)>,
    },
    /// Every key in the batch is now locked by this transaction.
    Prewrite,
    /// The commit records are written and the locks are gone.
    Commit,
    /// The rollback markers are written.
    Rollback,
    /// The stuck transaction's keys are finished.
    ResolveLock {
        /// How many keys were finished. A reader that asked for "every key of that
        /// transaction" learns whether there is more to do.
        resolved: u64,
    },
    /// The lock's TTL now in effect. It may be *longer* than what was asked for, because a
    /// store never shortens a lock; it is never shorter.
    Heartbeat {
        /// The TTL the lock now carries.
        ttl_ms: u64,
    },
    /// The safepoint now in effect. It may be higher than what was asked for, because a store
    /// never moves its safepoint backwards.
    GcSafepoint {
        /// The safepoint the store now holds.
        safepoint: u64,
    },
}

impl TxnKvResp {
    /// The method this is a response to.
    #[must_use]
    pub fn method(&self) -> Method {
        match self {
            Self::Get { .. } => Method::TxnGet,
            Self::Scan { .. } => Method::TxnScan,
            Self::Prewrite => Method::TxnPrewrite,
            Self::Commit => Method::TxnCommit,
            Self::Rollback => Method::TxnRollback,
            Self::ResolveLock { .. } => Method::TxnResolveLock,
            Self::Heartbeat { .. } => Method::TxnHeartbeat,
            Self::GcSafepoint { .. } => Method::TxnGcSafepoint,
        }
    }

    pub(crate) fn encode(&self, out: &mut Encoder) {
        match self {
            Self::Get { value } => out.put_opt_bytes(value.as_deref()),
            Self::Scan { pairs } => {
                out.put_varint(pairs.len() as u64);
                for (key, value) in pairs {
                    out.put_bytes(key);
                    out.put_bytes(value);
                }
            }
            // These three carry nothing: the store either did it or answered with an error
            // frame, and there is no third outcome to report.
            Self::Prewrite | Self::Commit | Self::Rollback => {}
            Self::ResolveLock { resolved } => out.put_varint(*resolved),
            Self::Heartbeat { ttl_ms } => out.put_varint(*ttl_ms),
            Self::GcSafepoint { safepoint } => out.put_varint(*safepoint),
        }
    }

    pub(crate) fn decode(method: Method, input: &mut Decoder<'_>) -> Result<Self, DecodeError> {
        let response = match method {
            Method::TxnGet => Self::Get {
                value: take_opt(input, "value")?,
            },
            Method::TxnScan => {
                let count = input.get_count("pairs")?;
                let mut pairs = Vec::with_capacity(count);
                for _ in 0..count {
                    pairs.push((take(input, "key")?, take(input, "value")?));
                }
                Self::Scan { pairs }
            }
            Method::TxnPrewrite => Self::Prewrite,
            Method::TxnCommit => Self::Commit,
            Method::TxnRollback => Self::Rollback,
            Method::TxnResolveLock => Self::ResolveLock {
                resolved: input.get_varint("resolved")?,
            },
            Method::TxnHeartbeat => Self::Heartbeat {
                ttl_ms: input.get_varint("ttl_ms")?,
            },
            Method::TxnGcSafepoint => Self::GcSafepoint {
                safepoint: input.get_varint("safepoint")?,
            },
            other => {
                return Err(DecodeError::invalid(
                    "method",
                    format!("{} is not a TxnKv method", other.name()),
                ));
            }
        };
        Ok(response)
    }
}

fn put_keys(out: &mut Encoder, keys: &[Bytes]) {
    out.put_varint(keys.len() as u64);
    for key in keys {
        out.put_bytes(key);
    }
}

fn get_keys(input: &mut Decoder<'_>) -> Result<Vec<Bytes>, DecodeError> {
    let count = input.get_count("keys")?;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        keys.push(take(input, "key")?);
    }
    Ok(keys)
}

fn take(input: &mut Decoder<'_>, field: &'static str) -> Result<Bytes, DecodeError> {
    Ok(Bytes::copy_from_slice(input.get_bytes(field)?))
}

fn take_opt(input: &mut Decoder<'_>, field: &'static str) -> Result<Option<Bytes>, DecodeError> {
    Ok(input.get_opt_bytes(field)?.map(Bytes::copy_from_slice))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::{LockInfo, TxnKvReq, TxnKvResp, TxnMutation};
    use crate::error::ProtoError;
    use crate::messages::{Method, SERVICE_TXN_KV};

    fn lock() -> LockInfo {
        LockInfo {
            key: Bytes::from_static(b"account/1"),
            primary: Bytes::from_static(b"account/0"),
            start_ts: 1 << 41,
            ttl_ms: 3_000,
        }
    }

    #[test]
    fn a_lock_round_trips_through_the_error_it_travels_in() {
        let error = lock().into_error();
        assert_eq!(LockInfo::from_error(&error).unwrap().unwrap(), lock());
        assert!(
            LockInfo::from_error(&ProtoError::NotBootstrapped).is_none(),
            "only a Locked carries one"
        );
    }

    /// A refusal a client cannot read is not a refusal it may treat as "nothing in the way".
    #[test]
    fn an_undecodable_lock_payload_is_an_error_not_an_absence() {
        let error = ProtoError::Locked {
            lock_info: Bytes::from_static(b"\xff\xff\xff"),
        };
        assert!(LockInfo::from_error(&error).unwrap().is_err());
    }

    /// Every method is in service 0x02, and the eight numbers are the order `docs/DESIGN.md`
    /// §9 lists them in. They are format: a collision or a renumbering breaks every peer.
    #[test]
    fn the_eight_methods_are_service_two_in_order() {
        let requests = [
            TxnKvReq::Get {
                key: Bytes::new(),
                ts: 1,
            },
            TxnKvReq::Scan {
                start: Bytes::new(),
                end: Bytes::new(),
                limit: 1,
                ts: 1,
                reverse: false,
            },
            TxnKvReq::Prewrite {
                start_ts: 1,
                primary: Bytes::new(),
                ttl_ms: 1,
                mutations: vec![],
            },
            TxnKvReq::Commit {
                start_ts: 1,
                commit_ts: 2,
                keys: vec![],
            },
            TxnKvReq::Rollback {
                start_ts: 1,
                keys: vec![],
            },
            TxnKvReq::ResolveLock {
                start_ts: 1,
                commit_ts: 0,
                keys: vec![],
            },
            TxnKvReq::Heartbeat {
                start_ts: 1,
                primary: Bytes::new(),
                ttl_ms: 1,
            },
            TxnKvReq::GcSafepoint { safepoint: 1 },
        ];
        for (index, request) in requests.iter().enumerate() {
            let method = request.method();
            assert_eq!(method.service(), SERVICE_TXN_KV, "{}", method.name());
            assert_eq!(
                method.as_u16(),
                0x0201 + u16::try_from(index).unwrap(),
                "{} is out of order",
                method.name()
            );
        }
    }

    /// Reads never change stored state; the other six do, including `Heartbeat` — extending a
    /// lock is a write, and a client that retried it as if it were free would be wrong about
    /// the one thing this distinction exists for.
    #[test]
    fn reads_are_not_mutations_and_everything_else_is() {
        assert!(!Method::TxnGet.is_mutation());
        assert!(!Method::TxnScan.is_mutation());
        for method in [
            Method::TxnPrewrite,
            Method::TxnCommit,
            Method::TxnRollback,
            Method::TxnResolveLock,
            Method::TxnHeartbeat,
            Method::TxnGcSafepoint,
        ] {
            assert!(method.is_mutation(), "{}", method.name());
        }
    }

    /// Every request names the key it routes by, and a batch with no keys routes to the start
    /// of the key space rather than panicking.
    #[test]
    fn every_request_routes_by_a_key() {
        assert_eq!(
            TxnKvReq::Get {
                key: Bytes::from_static(b"k"),
                ts: 1
            }
            .routing_key(),
            b"k"
        );
        assert_eq!(
            TxnKvReq::Prewrite {
                start_ts: 1,
                primary: Bytes::from_static(b"p"),
                ttl_ms: 1,
                mutations: vec![TxnMutation::Delete {
                    key: Bytes::from_static(b"a")
                }],
            }
            .routing_key(),
            b"a"
        );
        assert_eq!(
            TxnKvReq::Commit {
                start_ts: 1,
                commit_ts: 2,
                keys: vec![],
            }
            .routing_key(),
            b"",
            "an empty batch must not panic"
        );
        assert_eq!(
            TxnKvReq::Heartbeat {
                start_ts: 1,
                primary: Bytes::from_static(b"p"),
                ttl_ms: 1,
            }
            .routing_key(),
            b"p",
            "a heartbeat is addressed to the primary's region"
        );
    }

    /// A mutation tag this version does not define is an error, never a skipped entry.
    #[test]
    fn an_unknown_mutation_tag_is_an_error() {
        use crate::codec::{Decoder, Encoder};
        let mut out = Encoder::new();
        out.put_u8(9);
        out.put_bytes(b"k");
        let bytes = out.finish();
        assert!(TxnMutation::decode(&mut Decoder::new(&bytes)).is_err());
    }

    /// A response is self-describing: its tag is the method it answers.
    #[test]
    fn a_response_carries_the_method_it_answers() {
        for response in [
            TxnKvResp::Get { value: None },
            TxnKvResp::Scan { pairs: vec![] },
            TxnKvResp::Prewrite,
            TxnKvResp::Commit,
            TxnKvResp::Rollback,
            TxnKvResp::ResolveLock { resolved: 0 },
            TxnKvResp::Heartbeat { ttl_ms: 1 },
            TxnKvResp::GcSafepoint { safepoint: 1 },
        ] {
            let method = response.method();
            assert_eq!(method.service(), SERVICE_TXN_KV);
            assert_eq!(Method::from_u16(method.as_u16()), Some(method));
        }
    }
}
