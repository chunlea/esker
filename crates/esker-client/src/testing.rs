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
//! use esker_client::wire::{RawMethod, RawResponse};
//!
//! let transport = FakeTransport::new();
//! transport.script(Rule::new(
//!     Matcher::Method(RawMethod::Get),
//!     Outcome::Reply(RawResponse::Get(None)),
//! ));
//! ```

use std::sync::Mutex;
use std::time::Instant;

use bytes::Bytes;

use crate::transport::Transport;
use crate::wire::{
    CallError, CallResult, RawMethod, RawRequest, RawResponse, Request, RequestContext,
    ServerError, TransportError,
};

/// Which requests a rule answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Matcher {
    /// Every request.
    Any,
    /// Requests of one method.
    Method(RawMethod),
    /// Requests addressed to one store.
    Store(u64),
    /// Requests whose routing key is exactly these bytes.
    Key(Bytes),
    /// Requests for one region.
    Region(u64),
    /// Requests matching every one of these.
    All(Vec<Matcher>),
}

impl Matcher {
    /// Whether `request`, sent to `store_id`, is one this rule answers.
    #[must_use]
    pub fn matches(&self, store_id: u64, request: &Request) -> bool {
        match self {
            Self::Any => true,
            Self::Method(method) => request.body.method() == *method,
            Self::Store(id) => store_id == *id,
            Self::Key(key) => request.body.routing_key() == &key[..],
            Self::Region(id) => request.context.region_id == *id,
            Self::All(matchers) => matchers
                .iter()
                .all(|matcher| matcher.matches(store_id, request)),
        }
    }
}

/// What a matched rule does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Answer with this body.
    Reply(RawResponse),
    /// Refuse, the way a store refuses.
    Refuse(ServerError),
    /// Fail without an answer, the way a socket fails.
    Fail(TransportError),
}

impl Outcome {
    fn into_result(self) -> CallResult {
        match self {
            Self::Reply(response) => Ok(response),
            Self::Refuse(error) => Err(CallError::Server(error)),
            Self::Fail(error) => Err(CallError::Transport(error)),
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
    /// The routing header it carried.
    pub context: RequestContext,
    /// The body, exactly as the client built it.
    pub body: RawRequest,
}

#[derive(Debug)]
struct Inner {
    rules: Vec<Rule>,
    log: Vec<Call>,
    unmatched: Outcome,
    max_frame_size: usize,
}

/// A [`Transport`] that answers from a script and remembers everything it was asked.
///
/// Rules are tried front to back; the first live rule whose matcher matches answers the call
/// and spends one of its uses. A call that matches nothing gets [`FakeTransport::unmatched`],
/// which by default is a protocol error — a mis-scripted test then fails at once instead of
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
                unmatched: Outcome::Fail(TransportError::Protocol(
                    "fake transport: no rule matched".to_owned(),
                )),
                max_frame_size: esker_proto::MAX_FRAME_SIZE,
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
    /// [`Transport::call`] at every call site in this crate's tests.
    #[must_use]
    pub fn nth_call(&self, index: usize) -> Option<Call> {
        self.lock().log.get(index).cloned()
    }

    /// The methods of every call so far, which is what most assertions actually want.
    #[must_use]
    pub fn methods(&self) -> Vec<RawMethod> {
        self.lock()
            .log
            .iter()
            .map(|call| call.body.method())
            .collect()
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
    /// crate is single-threaded over its own transport. Recovering the guard keeps one
    /// failing assertion from turning into a second, confusing panic in the teardown.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Transport for FakeTransport {
    fn call(&self, store_id: u64, request: &Request, _deadline: Instant) -> CallResult {
        let mut inner = self.lock();
        inner.log.push(Call {
            store_id,
            context: request.context.clone(),
            body: request.body.clone(),
        });

        for rule in &mut inner.rules {
            if !rule.is_live() || !rule.matcher.matches(store_id, request) {
                continue;
            }
            if let Some(remaining) = rule.remaining.as_mut() {
                *remaining -= 1;
            }
            return rule.outcome.clone().into_result();
        }
        inner.unmatched.clone().into_result()
    }

    fn max_frame_size(&self) -> usize {
        self.lock().max_frame_size
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{Bytes, FakeTransport, Matcher, Outcome, Rule};
    use crate::transport::Transport;
    use crate::wire::{
        CallError, Peer, RawMethod, RawRequest, RawResponse, RegionEpoch, Request, RequestContext,
        ServerError, TransportError,
    };

    fn request(body: RawRequest) -> Request {
        Request {
            context: RequestContext {
                region_id: 1,
                epoch: RegionEpoch::default(),
                peer: Peer::voter(1, 1),
            },
            body,
        }
    }

    fn get(key: &'static [u8]) -> Request {
        request(RawRequest::Get {
            key: Bytes::from_static(key),
        })
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
                Outcome::Refuse(ServerError::ServerIsBusy {
                    reason: "stall".to_owned(),
                    backoff_ms: 5,
                }),
            ))
            .script(Rule::new(Matcher::Any, Outcome::Reply(RawResponse::Get(None))).forever());

        let first = transport.call(1, &get(b"k"), deadline());
        assert!(matches!(
            first,
            Err(CallError::Server(ServerError::ServerIsBusy { .. }))
        ));
        // The one-shot rule is spent, so the next call falls through to the one behind it.
        for _ in 0..3 {
            assert_eq!(
                transport.call(1, &get(b"k"), deadline()),
                Ok(RawResponse::Get(None))
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
                        Matcher::Method(RawMethod::Get),
                        Matcher::Store(2),
                        Matcher::Key(Bytes::from_static(b"wanted")),
                    ]),
                    Outcome::Reply(RawResponse::Get(Some(Bytes::from_static(b"v")))),
                )
                .forever(),
            )
            .unmatched(Outcome::Reply(RawResponse::Get(None)));

        assert_eq!(
            transport.call(2, &get(b"wanted"), deadline()),
            Ok(RawResponse::Get(Some(Bytes::from_static(b"v"))))
        );
        // Right key, wrong store.
        assert_eq!(
            transport.call(1, &get(b"wanted"), deadline()),
            Ok(RawResponse::Get(None))
        );
        // Right store, wrong key.
        assert_eq!(
            transport.call(2, &get(b"other"), deadline()),
            Ok(RawResponse::Get(None))
        );
        // Right store and key, wrong method.
        let put = request(RawRequest::Put {
            key: Bytes::from_static(b"wanted"),
            value: Bytes::from_static(b"v"),
        });
        assert_eq!(
            transport.call(2, &put, deadline()),
            Ok(RawResponse::Get(None))
        );
    }

    /// A script that does not cover a call is a broken test, and it should say so at once
    /// rather than look like a server that keeps failing.
    #[test]
    fn an_unmatched_call_is_a_protocol_error_by_default() {
        let transport = FakeTransport::new();
        let result = transport.call(1, &get(b"k"), deadline());
        assert!(matches!(
            result,
            Err(CallError::Transport(TransportError::Protocol(_)))
        ));
    }

    #[test]
    fn the_log_keeps_the_request_as_it_was_sent() {
        let transport = FakeTransport::new();
        transport.script(Rule::new(Matcher::Any, Outcome::Reply(RawResponse::Delete)).forever());
        let body = RawRequest::Delete {
            key: Bytes::from_static(b"raw-user-key"),
        };
        let _unused = transport.call(7, &request(body.clone()), deadline());

        let call = transport.nth_call(0).expect("one call was made");
        assert_eq!(call.store_id, 7);
        assert_eq!(call.body, body);
        assert_eq!(call.context.region_id, 1);
        assert_eq!(transport.methods(), vec![RawMethod::Delete]);

        transport.clear_log();
        assert_eq!(transport.call_count(), 0);
    }

    #[test]
    fn a_rule_can_be_spent_a_fixed_number_of_times() {
        let transport = FakeTransport::new();
        transport
            .script(
                Rule::new(
                    Matcher::Any,
                    Outcome::Fail(TransportError::NotSent("refused".to_owned())),
                )
                .times(2),
            )
            .script(Rule::new(Matcher::Any, Outcome::Reply(RawResponse::Get(None))).forever());

        for _ in 0..2 {
            assert!(matches!(
                transport.call(1, &get(b"k"), deadline()),
                Err(CallError::Transport(TransportError::NotSent(_)))
            ));
        }
        assert_eq!(
            transport.call(1, &get(b"k"), deadline()),
            Ok(RawResponse::Get(None))
        );
    }

    #[test]
    fn the_frame_limit_is_adjustable_for_tests() {
        let transport = FakeTransport::new();
        assert_eq!(transport.max_frame_size(), esker_proto::MAX_FRAME_SIZE);
        transport.set_max_frame_size(64);
        assert_eq!(transport.max_frame_size(), 64);
    }
}
