//! Which member of a placement-driver group a client believes leads it.
//!
//! A placement driver is a Raft group and **only its leader answers**
//! ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)), so every client of one holds a list
//! of members, believes one of them, and moves when it is told to. There are two such clients —
//! `esker_store::pd_remote::RemotePd`, which owns a dispatcher thread, and `esker_sql::pd::PdConn`,
//! which owns a [`crate::BlockingTransport`] — and they have nothing in common but this decision.
//!
//! So the decision lives here and the sockets do not. [`LeaderBook`] never performs a call: what it
//! knows is which member to talk to next, and the caller does the talking.
//! [ADR 0108](../../../docs/adr/0108-a-cluster-starts-n-placement-drivers-and-every-client-follows-the-leader.md)
//! is why it is in this crate: `PdNotLeader` is a wire error with an address in it, and what a
//! client must do when one arrives is as much this protocol's rule as the encoding of the error is.
//!
//! # The four rules
//!
//! * **The believed member is sticky.** The next call starts where the last one succeeded, so a
//!   cluster pays for a leader change once rather than on every call.
//! * **A hint that names a member** moves the book and the call is made again — under
//!   [`REDIRECT_BUDGET`], because a group mid-election hands out hints that chase each other and a
//!   client that followed them for ever would never fail.
//! * **A hint that names nobody** is an election in progress, and there is nothing to chase: the
//!   member's answer will not change until the election ends. The book moves to the next member and
//!   the caller waits, doubling from [`NO_LEADER_BACKOFF_MS`] to [`NO_LEADER_BACKOFF_MAX_MS`].
//! * **A hint naming an address the book does not hold** is *not* a misconfiguration on its own,
//!   because a placement driver's membership moves
//!   ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)):
//!   it may be a member added since this client started. So the book **refreshes** — the caller
//!   supplies the `Pd::Members` answer — and adopts the new list only if its **group id** is the
//!   one this book first learned. That check is what keeps ADR 0059's protection: one cluster's
//!   placement driver still cannot route a client into another's.
//!
//! # And a fifth, which the other four cannot reach
//!
//! **A killed member says nothing at all.** It does not answer `PdNotLeader`; it does not answer.
//! A client that moved only on a refusal has nothing to move it, so it re-dials the corpse on its
//! own cadence for ever while live members sit in its list with a leader between them — which is
//! the shape of debt #52 one layer up, where a client that never redialled a *store* that came
//! back cost 111 seconds of unavailability.
//!
//! So a member this client **could not reach** is advanced past too, under the same budget:
//! [`is_unreachable`] says which failures those are, and it is deliberately the narrowest
//! possible set. Only `ProtoError::NotSent` — a request that provably never left this process,
//! which is what a connection that could not be built is. A call that went out and lost its answer
//! is `Closed` or `Timeout`, and those are returned to the caller as they are: sending such a request
//! to a different member would be sending, a second time, a request that may already have applied.
//! The cost of that narrowness is one call — the one in flight when the socket died; the next call
//! has to build a connection, and that is the failure that moves the book.

use std::net::SocketAddr;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::ProtoError;
use crate::pd::PdMembership;

/// Redirects one call will follow before it gives up.
///
/// Twice round a group of three. A bound rather than a timeout because the failure it guards
/// against is a *loop* — two members each naming the other while an election settles — and a loop
/// is bounded by counting, not by waiting.
pub const REDIRECT_BUDGET: usize = 6;

/// How long a client waits when no member will say who leads.
///
/// An election takes one to two seconds at the project's tick (`esker_raft::TICK_MS`), so this is
/// short enough to catch the end of one and long enough that a client is not spinning through
/// three endpoints while it runs. It doubles up to [`NO_LEADER_BACKOFF_MAX_MS`], which is the
/// difference between waiting and hammering.
pub const NO_LEADER_BACKOFF_MS: u64 = 100;

/// The cap on that backoff.
pub const NO_LEADER_BACKOFF_MAX_MS: u64 = 800;

/// Whether `error` means *this member could not be reached*, rather than something it said.
///
/// The narrowest possible set, and the narrowness is the argument: `ProtoError::NotSent` is the
/// one failure that **provably never left this process**, so moving to another member and asking
/// again cannot be asking twice. Everything ambiguous — a connection that closed mid-call, a
/// request that timed out — is the caller's to report, because a client that re-sent those would
/// be repeating requests whose outcome it does not know.
#[must_use]
pub fn is_unreachable(error: &ProtoError) -> bool {
    matches!(error, ProtoError::NotSent { .. })
}

/// The members of one placement-driver group, and which of them a client believes leads.
///
/// Shared: `RemotePd`'s dispatcher reads [`LeaderBook::believed`] to decide which socket to hold,
/// while the calling thread moves it. Every field is behind a lock or an atomic for that reason.
#[derive(Debug)]
pub struct LeaderBook {
    /// Every member this client knows of, in the order it was given them.
    ///
    /// **It grows and shrinks**, because a placement driver's group does. The configured list is
    /// where it starts; a refresh replaces it with what the group says, once the group has proved
    /// it is the same group.
    endpoints: RwLock<Vec<SocketAddr>>,
    /// The group this client first learned, or zero before it has learned one.
    ///
    /// Latched, and never overwritten with a different value: it is what a refreshed member list is
    /// checked against, and a client that adopted a new id along with a new list would have checked
    /// nothing.
    group_id: AtomicU64,
    /// Which member this client believes leads, as an index into `endpoints`.
    at: AtomicUsize,
}

impl LeaderBook {
    /// A book over `endpoints`, believing the first of them.
    ///
    /// Refuses an empty list rather than defaulting to one: a client with nowhere to go is a
    /// configuration error, and finding out at the first call would name the wrong thing.
    pub fn new(endpoints: &[SocketAddr]) -> Result<Self, ProtoError> {
        if endpoints.is_empty() {
            return Err(ProtoError::internal(
                "a placement-driver client needs at least one address",
            ));
        }
        Ok(Self {
            endpoints: RwLock::new(endpoints.to_vec()),
            group_id: AtomicU64::new(0),
            at: AtomicUsize::new(0),
        })
    }

    /// A book over a single member — the group of one that a development cluster still is.
    ///
    /// Infallible, which is the point: there is no empty list to refuse, so a caller with one
    /// address does not have to carry a `Result` it can never see.
    #[must_use]
    pub fn lone(address: SocketAddr) -> Self {
        Self {
            endpoints: RwLock::new(vec![address]),
            group_id: AtomicU64::new(0),
            at: AtomicUsize::new(0),
        }
    }

    /// The member this client believes leads, and therefore the one to talk to.
    #[must_use]
    pub fn believed(&self) -> SocketAddr {
        let endpoints = self.endpoints();
        endpoints[self.at.load(Ordering::Acquire).min(endpoints.len() - 1)]
    }

    /// Every member this client knows of.
    ///
    /// A poisoned lock — a thread panicked holding it — answers with what was there rather than
    /// panicking again (`CLAUDE.md` invariant 9).
    #[must_use]
    pub fn endpoints(&self) -> Vec<SocketAddr> {
        self.endpoints.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |held| held.clone(),
        )
    }

    /// The group this client learned, or zero before it has learned one.
    #[must_use]
    pub fn group_id(&self) -> u64 {
        self.group_id.load(Ordering::Acquire)
    }

    /// Points this client at the member `hint` names, and says whether it moved.
    ///
    /// `members` is called **only** when the hint names an address this book does not hold, and it
    /// is the caller's `Pd::Members` call against the member that gave the hint — which is one of
    /// ours, and which any member answers. Keeping it a closure is what leaves the socket outside
    /// this crate's decision.
    ///
    /// An empty hint is an election in progress and moves nothing; the caller should
    /// [`advance`](Self::advance) and wait.
    pub fn follow<F>(&self, hint: &str, members: F) -> bool
    where
        F: FnOnce() -> Result<PdMembership, ProtoError>,
    {
        let Ok(hinted) = hint.parse::<SocketAddr>() else {
            return false;
        };
        if let Some(at) = self.position(hinted) {
            return self.at.swap(at, Ordering::AcqRel) != at;
        }
        // Not one of ours — which, since a placement driver's membership moves, is as likely to
        // mean "added since this client started" as "misconfigured".
        if !self.refresh(members) {
            return false;
        }
        let Some(at) = self.position(hinted) else {
            tracing::warn!(
                %hinted,
                "the placement driver named a leader its own group does not contain"
            );
            return false;
        };
        self.at.swap(at, Ordering::AcqRel) != at
    }

    /// Adopts a membership answer, and says whether it was **accepted**.
    ///
    /// Accepted, and not *changed*: an answer that confirms the list this book already holds is a
    /// good answer, and a caller reading "false" as "that group would not confirm it" would then
    /// refuse to follow a leader the group had just vouched for. Whether the list moved is this
    /// method's own business — it is what resets the believed index and writes the log line.
    ///
    /// **The group id is the guard**, and it is the whole of what makes following an unknown hint
    /// safe. Before ADR 0061 such a hint was refused outright, because following one would let
    /// another cluster's placement driver route this client. That refusal cannot stand once
    /// membership moves — the address may be a member added an hour ago — so the check moves with
    /// it: a list is adopted only from a group whose id is the one this book first learned.
    ///
    /// The first answer latches the id. There is nothing to compare it against, and nothing to gain
    /// from refusing it: this client was pointed at that address by its own configuration.
    pub fn adopt(&self, membership: &PdMembership) -> bool {
        let known = self.group_id.load(Ordering::Acquire);
        if known != 0 && membership.group_id != known {
            tracing::error!(
                expected = format_args!("{known:#018x}"),
                actual = format_args!("{:#018x}", membership.group_id),
                "a placement driver answered for a different group; its members were not adopted"
            );
            return false;
        }
        let mut fresh = Vec::with_capacity(membership.members.len());
        for member in &membership.members {
            match member.address.parse::<SocketAddr>() {
                Ok(address) => fresh.push(address),
                // A member with an address this build cannot parse is one it cannot reach, and
                // dropping it is better than refusing the whole list: the others are still good.
                Err(error) => tracing::warn!(
                    id = member.id,
                    address = %member.address,
                    %error,
                    "a placement driver's address will not parse; skipping it"
                ),
            }
        }
        if fresh.is_empty() {
            return false;
        }
        self.group_id.store(membership.group_id, Ordering::Release);
        let moved = {
            let Ok(mut held) = self.endpoints.write() else {
                return false;
            };
            let moved = *held != fresh;
            *held = fresh;
            moved
        };
        if moved {
            // The index may now point past the end, or at a different member. Start again from the
            // top: the next refusal will say where to go, and one extra round trip after a
            // membership change is not worth remembering a position through it.
            self.at.store(0, Ordering::Release);
            tracing::info!(
                group_id = format_args!("{:#018x}", membership.group_id),
                members = membership.members.len(),
                "the placement-driver group has changed; adopting its members"
            );
        }
        true
    }

    /// Moves to the next member, for when nobody will say who leads.
    pub fn advance(&self) {
        let count = self.endpoints().len().max(1);
        let next = (self.at.load(Ordering::Acquire) + 1) % count;
        self.at.store(next, Ordering::Release);
    }

    fn position(&self, address: SocketAddr) -> Option<usize> {
        self.endpoints().iter().position(|end| *end == address)
    }

    fn refresh<F>(&self, members: F) -> bool
    where
        F: FnOnce() -> Result<PdMembership, ProtoError>,
    {
        match members() {
            Ok(membership) => self.adopt(&membership),
            Err(error) => {
                tracing::debug!(%error, "could not refresh the placement-driver group");
                false
            }
        }
    }
}

/// One call's allowance of redirects, and how long it waits when nobody will say who leads.
///
/// A per-call value rather than state on the book: the budget is about *this* request, and a book
/// that remembered it would refuse the second call of a cluster that had settled.
#[derive(Debug, Clone, Copy)]
pub struct Redirects {
    left: usize,
    backoff_ms: u64,
}

impl Redirects {
    /// A fresh allowance: [`REDIRECT_BUDGET`] redirects, waiting from [`NO_LEADER_BACKOFF_MS`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            left: REDIRECT_BUDGET,
            backoff_ms: NO_LEADER_BACKOFF_MS,
        }
    }

    /// Spends one redirect. `false` means the budget is gone and the caller owes its refusal to
    /// whoever asked.
    pub fn take(&mut self) -> bool {
        if self.left == 0 {
            return false;
        }
        self.left -= 1;
        true
    }

    /// How long to wait before asking the next member, doubling to [`NO_LEADER_BACKOFF_MAX_MS`].
    pub fn backoff(&mut self) -> Duration {
        let waiting = Duration::from_millis(self.backoff_ms);
        self.backoff_ms = (self.backoff_ms * 2).min(NO_LEADER_BACKOFF_MAX_MS);
        waiting
    }
}

impl Default for Redirects {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{LeaderBook, NO_LEADER_BACKOFF_MAX_MS, REDIRECT_BUDGET, Redirects};
    use crate::ProtoError;
    use crate::pd::{PdMemberInfo, PdMembership, PdRole};
    use std::net::SocketAddr;
    use std::time::Duration;

    const GROUP: u64 = 0x0102_0304_0506_0708;

    fn address(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    fn three() -> LeaderBook {
        LeaderBook::new(&[address(1), address(2), address(3)]).unwrap()
    }

    fn membership(group_id: u64, ports: &[u16]) -> PdMembership {
        PdMembership {
            group_id,
            this_id: 1,
            leader_id: 1,
            term: 1,
            members: ports
                .iter()
                .enumerate()
                .map(|(at, port)| PdMemberInfo {
                    id: at as u64 + 1,
                    address: address(*port).to_string(),
                    role: PdRole::Voter,
                })
                .collect(),
        }
    }

    /// Refused, and not left to fail at the first call: a client with nowhere to go is a
    /// configuration error and the message should name that.
    #[test]
    fn a_book_over_no_members_is_refused() {
        assert!(LeaderBook::new(&[]).is_err());
    }

    /// A hint that names a member moves the book, and the next call starts there — the stickiness
    /// that makes a cluster pay for a leader change once.
    #[test]
    fn a_hint_that_names_a_member_moves_the_book_and_stays() {
        let book = three();
        assert_eq!(book.believed(), address(1));
        assert!(book.follow(&address(3).to_string(), refuse));
        assert_eq!(book.believed(), address(3));
        // The same hint again changes nothing, and says so.
        assert!(!book.follow(&address(3).to_string(), refuse));
        assert_eq!(book.believed(), address(3));
    }

    /// An election in progress: nothing to chase, and nothing moved. The caller advances and waits.
    #[test]
    fn a_hint_that_names_nobody_moves_nothing() {
        let book = three();
        assert!(!book.follow("", refuse));
        assert_eq!(book.believed(), address(1));
        book.advance();
        assert_eq!(book.believed(), address(2));
        book.advance();
        book.advance();
        assert_eq!(book.believed(), address(1), "advancing wraps");
    }

    /// **The ADR 0061 rule.** An address outside the list is followed — but only after the group
    /// that gave it has been asked, and only because its id is the one this book learned.
    #[test]
    fn a_hint_outside_the_list_is_followed_after_the_group_confirms_it() {
        let book = three();
        // Learn the group first, as a client does on its first refresh. The list is the one it
        // already holds, and the answer is still accepted — that is what latches the id.
        assert!(book.adopt(&membership(GROUP, &[1, 2, 3])));
        assert_eq!(book.group_id(), GROUP);

        let asked = std::cell::Cell::new(0);
        let moved = book.follow(&address(4).to_string(), || {
            asked.set(asked.get() + 1);
            Ok(membership(GROUP, &[1, 2, 3, 4]))
        });
        assert!(moved, "the new member was adopted and followed");
        assert_eq!(asked.get(), 1, "the group is asked once, not per member");
        assert_eq!(book.believed(), address(4));
        assert_eq!(book.endpoints().len(), 4);
    }

    /// **The guard ADR 0059 left behind.** A different group's answer is refused, so one cluster's
    /// placement driver cannot route a client into another's — which is exactly what following an
    /// unknown address would otherwise allow.
    #[test]
    fn a_hint_confirmed_by_a_different_group_is_refused() {
        let book = three();
        assert!(book.adopt(&membership(GROUP, &[1, 2, 3])));

        let moved = book.follow(&address(4).to_string(), || {
            Ok(membership(GROUP ^ 0xffff, &[1, 2, 3, 4]))
        });
        assert!(!moved);
        assert_eq!(book.believed(), address(1), "the book did not move");
        assert_eq!(book.endpoints().len(), 3, "and did not adopt the list");
        assert_eq!(book.group_id(), GROUP, "and kept the id it had");
    }

    /// The group answered, its id matched, and it still does not contain the address it named.
    /// Nothing to follow, and the book stays where it was.
    #[test]
    fn a_hint_the_group_itself_does_not_contain_is_refused() {
        let book = three();
        let moved = book.follow(&address(9).to_string(), || {
            Ok(membership(GROUP, &[1, 2, 3]))
        });
        assert!(!moved);
        assert_eq!(book.believed(), address(1));
    }

    /// A refresh that cannot be made is not a reason to move: the call failed, and the book knows
    /// nothing new.
    #[test]
    fn a_refresh_that_fails_moves_nothing() {
        let book = three();
        assert!(!book.follow(&address(4).to_string(), refuse));
        assert_eq!(book.believed(), address(1));
        assert_eq!(book.endpoints().len(), 3);
    }

    /// A member whose address will not parse is dropped and the rest of the list is kept: the
    /// others are still reachable, and refusing all of them would strand the client.
    #[test]
    fn a_member_with_an_unusable_address_is_dropped_and_the_others_kept() {
        let book = three();
        let mut answer = membership(GROUP, &[1, 2]);
        answer.members.push(PdMemberInfo {
            id: 3,
            address: "not-an-address".to_owned(),
            role: PdRole::Voter,
        });
        assert!(book.adopt(&answer));
        assert_eq!(book.endpoints(), vec![address(1), address(2)]);
    }

    /// The budget is spent by counting, not by waiting, and the backoff doubles to its cap — the
    /// two numbers that stop a group mid-election from being hammered or chased for ever.
    #[test]
    fn the_allowance_is_spent_by_counting_and_the_wait_doubles_to_its_cap() {
        let mut redirects = Redirects::new();
        for _ in 0..REDIRECT_BUDGET {
            assert!(redirects.take());
        }
        assert!(!redirects.take(), "the budget is a bound, not a timeout");

        let mut waiting = Redirects::new();
        let mut last = waiting.backoff();
        for _ in 0..12 {
            let next = waiting.backoff();
            assert!(next >= last);
            last = next;
        }
        assert_eq!(last, Duration::from_millis(NO_LEADER_BACKOFF_MAX_MS));
    }

    /// **Rule five, and the line it draws.** A connection that could not be built provably sent
    /// nothing, so another member may be asked. A call that went out and lost its answer did not,
    /// so it may not — which is the whole reason this is a predicate and not "any error".
    #[test]
    fn only_a_request_that_never_left_moves_the_client() {
        assert!(super::is_unreachable(&ProtoError::not_sent(
            "connecting to 127.0.0.1:2379: Connection refused"
        )));
        for ambiguous in [
            ProtoError::Closed {
                detail: "mid-call".to_owned(),
            },
            ProtoError::Timeout {
                detail: "no answer".to_owned(),
            },
            ProtoError::Io {
                detail: "write failed".to_owned(),
            },
            ProtoError::internal("something else"),
        ] {
            assert!(
                !super::is_unreachable(&ambiguous),
                "{ambiguous} may have been applied; asking elsewhere would ask twice"
            );
        }
    }

    fn refuse() -> Result<PdMembership, ProtoError> {
        Err(ProtoError::internal("no membership in this test"))
    }
}
