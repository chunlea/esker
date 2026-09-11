//! A transport made of a script and a logbook.
//!
//! Every retry rule in this crate is a claim about what happens after a particular answer
//! comes back, and there is no way to get a real server to produce `NotLeader` four times and
//! then succeed. So the tests do not use one: [`FakeTransport`] is handed a queue of rules —
//! *this matcher answers with that outcome, this many times* — and records every call it was
//! given, so a test can assert on the requests as well as on the result.
//!
//! Two properties make it worth more than a hand-rolled mock per test:
//!
//! * **It records what went out.** The client is required never to namespace a key
//!   (`prompts/02-single-node-server.md`), and the only way to check a negative like that is
//!   to look at the bytes that reached the wire.
//! * **It never sleeps.** Paired with [`crate::clock::FakeClock`], a test that exercises a
//!   full retry budget with a two-second ceiling finishes in microseconds and asserts the
//!   exact backoff sequence rather than a lower bound on wall-clock time.
//!
//! ```
//! use esker_client::testing::{FakeTransport, Matcher, Outcome, Rule};
//! use esker_client::wire::{Method, RawKvResp};
//!
//! let transport = FakeTransport::new();
//! transport.script(Rule::new(
//!     Matcher::Method(Method::RawGet),
//!     Outcome::Reply(RawKvResp::Get { value: None }),
//! ));
//! ```

use std::sync::Mutex;
use std::time::Instant;

use bytes::Bytes;

use crate::transport::StoreTransport;
use crate::wire::{
    CallResult, LockInfo, Method, ProtoError, RawKvReq, RawKvResp, Request, RequestHeader,
    Response, TxnKvReq, TxnKvResp, routing_key,
};

pub use esker_proto::fragment::FragmentResp;

/// The `TxnKv` body of a request, when it has one.
#[must_use]
fn txn_body(request: &Request) -> Option<&TxnKvReq> {
    match request {
        Request::TxnKv { request, .. } => Some(request),
        Request::Hello(_)
        | Request::Raft(_)
        | Request::Snapshot(_)
        | Request::Pd { .. }
        | Request::Admin(_)
        // A fragment has a body, but not this one: it is a plan for a columnar replica, and this
        // crate carries it without interpreting it ([`esker_proto::fragment`]).
        | Request::Fragment { .. }
        // A schema fetch is store-to-store: one store asking another for a catalog record it
        // cannot read itself ([`esker_proto::schema`]). A client never sends one.
        | Request::Schema(_)
        | Request::RawKv { .. } => None,
    }
}

/// The key a request routes by, whichever service it belongs to.
#[must_use]
fn routed_key(request: &Request) -> Option<&[u8]> {
    match request {
        Request::RawKv { request, .. } => Some(routing_key(request)),
        Request::TxnKv { request, .. } => Some(request.routing_key()),
        Request::Hello(_)
        | Request::Raft(_)
        | Request::Snapshot(_)
        | Request::Pd { .. }
        // An operator's request names a region by id and carries no key, so no rule written in
        // terms of keys can match one. A fragment addresses a region the same way: its key range
        // is inside the opaque fragment bytes, which this crate does not decode.
        | Request::Admin(_)
        | Request::Schema(_)
        | Request::Fragment { .. } => None,
    }
}

/// The `RawKv` body of a request, when it has one.
#[must_use]
fn raw_body(request: &Request) -> Option<&RawKvReq> {
    match request {
        Request::RawKv { request, .. } => Some(request),
        // A client never sends `Hello` through a rule — the transport handles it — and never
        // sends Raft traffic at all: that is store-to-store, on connections a client has none of.
        // A `Pd` request has no `RawKv` body and is not addressed to a region, so no rule
        // written in terms of keys or regions can match one; a snapshot request is a follower
        // asking a leader, which is store-to-store as well. A `TxnKv` request has a body, but
        // not this one — `txn_body` above is its shape, and `routed_key` covers both.
        Request::Hello(_)
        | Request::Raft(_)
        | Request::Snapshot(_)
        | Request::Pd { .. }
        | Request::Admin(_)
        | Request::Fragment { .. }
        | Request::Schema(_)
        | Request::TxnKv { .. } => None,
    }
}

/// Which requests a rule answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Every request.
    Any,
    /// Requests of one method.
    Method(Method),
    /// Requests addressed to one store.
    Store(u64),
    /// Requests whose routing key is exactly these bytes.
    Key(Bytes),
    /// Requests for one region.
    Region(u64),
    /// Requests addressed to one peer.
    Peer(u64),
    /// Requests matching every one of these.
    All(Vec<Matcher>),
}

impl Matcher {
    /// Whether `request`, sent to `store_id`, is one this rule answers.
    #[must_use]
    pub fn matches(&self, store_id: u64, request: &Request) -> bool {
        match self {
            Self::Any => true,
            Self::Method(method) => request.method() == *method,
            Self::Store(id) => store_id == *id,
            Self::Key(key) => routed_key(request).is_some_and(|routed| routed == &key[..]),
            Self::Region(id) => request
                .header()
                .is_some_and(|header| header.region_id == *id),
            Self::Peer(id) => request.header().is_some_and(|header| header.peer == *id),
            Self::All(matchers) => matchers
                .iter()
                .all(|matcher| matcher.matches(store_id, request)),
        }
    }
}

/// What a matched rule does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Answer with this `RawKv` body.
    Reply(RawKvResp),
    /// Answer with this `TxnKv` body (`docs/DESIGN.md` §8).
    TxnReply(TxnKvResp),
    /// Answer a fragment (`docs/adr/0022-columnar-learner-replica.md` Decision 3).
    ///
    /// A [`FragmentResp::Refused`] goes here rather than in [`Outcome::Fail`], because that is
    /// where a real store puts it: a refusal is a *response* variant and not an error frame, and a
    /// fake that raised it as an error would let a client pass its tests by treating a normal
    /// answer as a fault.
    FragmentReply(FragmentResp),
    /// Answer a `Prewrite` with "every key locked", sized from the request.
    ///
    /// A `Prewrite` answers one status per mutation
    /// ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md) decision 1), and a client that
    /// checks the length — this one does — would refuse a fixed-size answer to a batch of a
    /// different size. Counting the request's mutations is what a store does, so the fake does
    /// it too rather than making every test spell the number out.
    PrewriteOk,
    /// Fail with this error — a refusal from the store, or a socket that gave up. One enum
    /// covers both because `esker-proto` does: what separates them is
    /// [`ProtoError::outcome`], not which layer raised it.
    Fail(ProtoError),
}

impl Outcome {
    /// Answer a locked key the way a store does: an `Error` frame carrying the lock, so the
    /// client's resolution path is driven through the same channel a real store uses
    /// ([ADR 0016](../../../docs/adr/0016-txnkv-on-the-wire.md)).
    #[must_use]
    pub fn locked(lock: &LockInfo) -> Self {
        Self::Fail(lock.into_error())
    }

    fn into_result(self, request: &Request) -> CallResult {
        match self {
            Self::Reply(response) => Ok(Response::RawKv(response)),
            Self::TxnReply(response) => Ok(Response::TxnKv(response)),
            Self::FragmentReply(response) => Ok(Response::Fragment(response)),
            Self::PrewriteOk => {
                let count = match txn_body(request) {
                    Some(TxnKvReq::Prewrite { mutations, .. }) => mutations.len(),
                    _ => 0,
                };
                Ok(Response::TxnKv(TxnKvResp::prewrite_ok(count)))
            }
            Self::Fail(error) => Err(error),
        }
    }
}

/// One entry of the script: an outcome, the requests it applies to, and how many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    matcher: Matcher,
    outcome: Outcome,
    /// Calls this rule may still answer; `None` is unlimited.
    remaining: Option<u32>,
}

impl Rule {
    /// A rule that answers **one** call. The common case: the interesting scripts are the
    /// ones that change their answer, and a rule that fires forever cannot.
    #[must_use]
    pub fn new(matcher: Matcher, outcome: Outcome) -> Self {
        Self {
            matcher,
            outcome,
            remaining: Some(1),
        }
    }

    /// Answers the next `count` matching calls.
    #[must_use]
    pub fn times(mut self, count: u32) -> Self {
        self.remaining = Some(count);
        self
    }

    /// Answers every matching call, without running out.
    #[must_use]
    pub fn forever(mut self) -> Self {
        self.remaining = None;
        self
    }

    /// Whether this rule has calls left in it.
    #[must_use]
    pub fn is_live(&self) -> bool {
        self.remaining.is_none_or(|remaining| remaining > 0)
    }
}

/// One call the transport was asked to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The store it was addressed to.
    pub store_id: u64,
    /// The request, exactly as the client built it.
    pub request: Request,
}

impl Call {
    /// The `RawKv` body, when this was a key-value call.
    #[must_use]
    pub fn body(&self) -> Option<&RawKvReq> {
        raw_body(&self.request)
    }

    /// The `TxnKv` body, when this was a transactional call.
    #[must_use]
    pub fn txn_body(&self) -> Option<&TxnKvReq> {
        txn_body(&self.request)
    }

    /// The routing header, when this was a key-value call.
    #[must_use]
    pub fn header(&self) -> Option<RequestHeader> {
        self.request.header()
    }

    /// The key the client routed by, whichever service this was.
    #[must_use]
    pub fn key(&self) -> Option<&[u8]> {
        routed_key(&self.request)
    }
}

#[derive(Debug)]
struct Inner {
    rules: Vec<Rule>,
    log: Vec<Call>,
    unmatched: Outcome,
    max_frame_size: usize,
}

/// A [`StoreTransport`] that answers from a script and remembers everything it was asked.
///
/// Rules are tried front to back; the first live rule whose matcher matches answers the call
/// and spends one of its uses. A call that matches nothing gets [`FakeTransport::unmatched`],
/// which by default is an internal error — a mis-scripted test then fails at once instead of
/// looping through a retry budget.
#[derive(Debug)]
pub struct FakeTransport {
    inner: Mutex<Inner>,
}

impl Default for FakeTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTransport {
    /// An empty script.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                rules: Vec::new(),
                log: Vec::new(),
                unmatched: Outcome::Fail(ProtoError::internal("fake transport: no rule matched")),
                max_frame_size: crate::wire::MAX_FRAME_SIZE,
            }),
        }
    }

    /// Appends a rule to the script.
    pub fn script(&self, rule: Rule) -> &Self {
        self.lock().rules.push(rule);
        self
    }

    /// Appends several rules, in order.
    pub fn script_all<I: IntoIterator<Item = Rule>>(&self, rules: I) -> &Self {
        self.lock().rules.extend(rules);
        self
    }

    /// Sets what happens to a call no rule matches.
    pub fn unmatched(&self, outcome: Outcome) -> &Self {
        self.lock().unmatched = outcome;
        self
    }

    /// Shrinks the frame limit, so a size check can be exercised without building sixteen
    /// megabytes of value.
    pub fn set_max_frame_size(&self, bytes: usize) -> &Self {
        self.lock().max_frame_size = bytes;
        self
    }

    /// Every call made so far, oldest first.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.lock().log.clone()
    }

    /// How many calls have been made.
    #[must_use]
    pub fn call_count(&self) -> usize {
        self.lock().log.len()
    }

    /// The `n`th call, if it happened.
    ///
    /// Named `nth_call` and not `call` because an inherent method of that name would shadow
    /// [`StoreTransport::call`] at every call site in this crate's tests.
    #[must_use]
    pub fn nth_call(&self, index: usize) -> Option<Call> {
        self.lock().log.get(index).cloned()
    }

    /// The methods of every call so far, which is what most assertions actually want.
    #[must_use]
    pub fn methods(&self) -> Vec<Method> {
        self.lock()
            .log
            .iter()
            .map(|call| call.request.method())
            .collect()
    }

    /// The peer each call was addressed to, which is how a `NotLeader` redirect is checked.
    #[must_use]
    pub fn peers(&self) -> Vec<u64> {
        self.lock()
            .log
            .iter()
            .filter_map(|call| call.header().map(|header| header.peer))
            .collect()
    }

    /// The store each call was addressed to.
    #[must_use]
    pub fn stores(&self) -> Vec<u64> {
        self.lock().log.iter().map(|call| call.store_id).collect()
    }

    /// Rules that still have uses left.
    #[must_use]
    pub fn live_rules(&self) -> usize {
        self.lock()
            .rules
            .iter()
            .filter(|rule| rule.is_live())
            .count()
    }

    /// Forgets the call log, keeping the script.
    pub fn clear_log(&self) -> &Self {
        self.lock().log.clear();
        self
    }

    /// A `Mutex` is only poisoned by a panic in another test thread, and every test in this
    /// crate is single-threaded over its own transport. Recovering the guard keeps one failing
    /// assertion from turning into a second, confusing panic in the teardown.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl StoreTransport for FakeTransport {
    fn call(&self, store_id: u64, request: &Request, _deadline: Instant) -> CallResult {
        let mut inner = self.lock();
        inner.log.push(Call {
            store_id,
            request: request.clone(),
        });

        for rule in &mut inner.rules {
            if !rule.is_live() || !rule.matcher.matches(store_id, request) {
                continue;
            }
            if let Some(remaining) = rule.remaining.as_mut() {
                *remaining -= 1;
            }
            return rule.outcome.clone().into_result(request);
        }
        inner.unmatched.clone().into_result(request)
    }

    fn max_frame_size(&self) -> usize {
        self.lock().max_frame_size
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Bytes, FakeTransport, Matcher, Outcome, Rule};
    use crate::transport::StoreTransport;
    use crate::wire::{
        Epoch, Method, ProtoError, RawKvReq, RawKvResp, Request, RequestHeader, Response,
    };

    fn request(body: RawKvReq) -> Request {
        Request::raw_kv(RequestHeader::new(1, Epoch::INITIAL, 1), body)
    }

    fn get(key: &'static [u8]) -> Request {
        request(RawKvReq::get(Bytes::from_static(key)))
    }

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    #[test]
    fn rules_answer_in_order_and_run_out() {
        let transport = FakeTransport::new();
        transport
            .script(Rule::new(
                Matcher::Any,
                Outcome::Fail(ProtoError::ServerIsBusy {
                    reason: "stall".to_owned(),
                }),
            ))
            .script(
                Rule::new(Matcher::Any, Outcome::Reply(RawKvResp::Get { value: None })).forever(),
            );

        let first = transport.call(1, &get(b"k"), deadline());
        assert!(matches!(first, Err(ProtoError::ServerIsBusy { .. })));
        // The one-shot rule is spent, so the next call falls through to the one behind it.
        for _ in 0..3 {
            assert_eq!(
                transport.call(1, &get(b"k"), deadline()),
                Ok(Response::RawKv(RawKvResp::Get { value: None }))
            );
        }
        assert_eq!(transport.call_count(), 4);
        assert_eq!(transport.live_rules(), 1);
    }

    #[test]
    fn a_matcher_can_be_narrowed_to_one_method_store_and_key() {
        let transport = FakeTransport::new();
        transport
            .script(
                Rule::new(
                    Matcher::All(vec![
                        Matcher::Method(Method::RawGet),
                        Matcher::Store(2),
                        Matcher::Key(Bytes::from_static(b"wanted")),
                    ]),
                    Outcome::Reply(RawKvResp::Get {
                        value: Some(Bytes::from_static(b"v")),
                    }),
                )
                .forever(),
            )
            .unmatched(Outcome::Reply(RawKvResp::Get { value: None }));

        let hit = Response::RawKv(RawKvResp::Get {
            value: Some(Bytes::from_static(b"v")),
        });
        let miss = Response::RawKv(RawKvResp::Get { value: None });
        assert_eq!(transport.call(2, &get(b"wanted"), deadline()), Ok(hit));
        // Right key, wrong store.
        assert_eq!(
            transport.call(1, &get(b"wanted"), deadline()),
            Ok(miss.clone())
        );
        // Right store, wrong key.
        assert_eq!(
            transport.call(2, &get(b"other"), deadline()),
            Ok(miss.clone())
        );
        // Right store and key, wrong method.
        let put = request(RawKvReq::put(
            Bytes::from_static(b"wanted"),
            Bytes::from_static(b"v"),
        ));
        assert_eq!(transport.call(2, &put, deadline()), Ok(miss));
    }

    /// A script that does not cover a call is a broken test, and it should say so at once
    /// rather than look like a server that keeps failing.
    #[test]
    fn an_unmatched_call_is_an_internal_error_by_default() {
        let transport = FakeTransport::new();
        let result = transport.call(1, &get(b"k"), deadline());
        assert!(matches!(result, Err(ProtoError::Internal { .. })));
    }

    #[test]
    fn the_log_keeps_the_request_as_it_was_sent() {
        let transport = FakeTransport::new();
        transport.script(Rule::new(Matcher::Any, Outcome::Reply(RawKvResp::Delete)).forever());
        let body = RawKvReq::delete(Bytes::from_static(b"raw-user-key"));
        let _unused = transport.call(7, &request(body.clone()), deadline());

        let call = transport.nth_call(0).expect("one call was made");
        assert_eq!(call.store_id, 7);
        assert_eq!(call.body(), Some(&body));
        assert_eq!(call.key(), Some(&b"raw-user-key"[..]));
        assert_eq!(call.header().map(|header| header.region_id), Some(1));
        assert_eq!(transport.methods(), vec![Method::RawDelete]);
        assert_eq!(transport.stores(), vec![7]);
        assert_eq!(transport.peers(), vec![1]);

        transport.clear_log();
        assert_eq!(transport.call_count(), 0);
    }

    #[test]
    fn a_rule_can_be_spent_a_fixed_number_of_times() {
        let transport = FakeTransport::new();
        transport
            .script(
                Rule::new(Matcher::Any, Outcome::Fail(ProtoError::not_sent("refused"))).times(2),
            )
            .script(
                Rule::new(Matcher::Any, Outcome::Reply(RawKvResp::Get { value: None })).forever(),
            );

        for _ in 0..2 {
            assert!(matches!(
                transport.call(1, &get(b"k"), deadline()),
                Err(ProtoError::NotSent { .. })
            ));
        }
        assert_eq!(
            transport.call(1, &get(b"k"), deadline()),
            Ok(Response::RawKv(RawKvResp::Get { value: None }))
        );
    }

    #[test]
    fn the_frame_limit_is_adjustable_for_tests() {
        let transport = FakeTransport::new();
        assert_eq!(transport.max_frame_size(), crate::wire::MAX_FRAME_SIZE);
        transport.set_max_frame_size(64);
        assert_eq!(transport.max_frame_size(), 64);
    }
}
