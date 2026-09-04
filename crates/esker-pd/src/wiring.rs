//! The connections this member holds, rebuilt when the group changes.
//!
//! # Why this exists, and why it should not for long
//!
//! [`PdTcpTransport`](crate::transport::PdTcpTransport) builds one queue and one delivery task per
//! peer **at `spawn`**, and dynamic membership is exactly the peer set changing
//! ([ADR 0061](../../../docs/adr/0061-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)).
//! The right shape is a `reconfigure` on that type, adding and dropping one queue per changed
//! member and leaving the rest connected — and `crates/esker-pd/src/transport.rs` belongs to the
//! `tls` lane until its RPC unit lands, so this is the shape that needs no edit there:
//! **hold the transport, and replace it.**
//!
//! What that costs is every connection on every change, including to members that did not move.
//! It is affordable for exactly two reasons and it is worth naming both, because neither is
//! permanent: a membership change is an operator action rather than a thing that happens under
//! load, and Raft retransmits whatever was in flight — a dropped message is indistinguishable from
//! a slow one. `docs/plans/phase-15-pd-ha.md` §11.7 is the note for the lane that should fold this
//! back in.
//!
//! # The group id is not rebuilt
//!
//! The replacement transport is built from a [`MemberList`] that carries the group's **recorded**
//! id, so every batch it stamps says the same thing the old one did. That is the whole reason
//! `transport.rs` needs no change: it reads `members.group_id()`, and under ADR 0061 that method
//! answers the recorded id rather than a derivation that would move as the membership did.

use std::sync::{Arc, RwLock};

use esker_proto::TransportConfig;
use esker_raft::{Message, NodeId};

use crate::driver::PdTransport;
use crate::error::{PdError, Result};
use crate::member::MemberList;
use crate::transport::PdTcpTransport;

/// A transport whose membership can move.
#[derive(Debug)]
pub struct PdWiring {
    id: NodeId,
    config: TransportConfig,
    /// The runtime the delivery tasks live on.
    ///
    /// Held because [`PdWiring::reconfigure`] is called from the **driver thread**, which is a
    /// plain OS thread with no runtime of its own — `tokio::spawn` there is a panic. This is the
    /// same reason `esker_store::peer::RaftPeer::spawn_ticker_on` takes one.
    runtime: tokio::runtime::Handle,
    inner: RwLock<Arc<PdTcpTransport>>,
    /// What `inner` was built for, so a change that changes nothing rebuilds nothing.
    members: RwLock<MemberList>,
}

impl PdWiring {
    /// Connects to every member of `members` but `id`.
    ///
    /// Must be called from inside a `tokio` runtime, whose handle it keeps.
    pub fn spawn(id: NodeId, members: &MemberList, config: TransportConfig) -> Result<Arc<Self>> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            PdError::internal(format!(
                "the placement driver's transport needs a runtime to spawn on: {error}"
            ))
        })?;
        let inner = PdTcpTransport::spawn(id, members, config)?;
        Ok(Arc::new(Self {
            id,
            config,
            runtime,
            inner: RwLock::new(inner),
            members: RwLock::new(members.clone()),
        }))
    }

    /// The membership this wiring currently reaches.
    #[must_use]
    pub fn members(&self) -> MemberList {
        self.members
            .read()
            .map_or_else(|_| MemberList::alone(self.id), |held| held.clone())
    }

    /// Rebuilds the connections for `members`, if they are not the ones already held.
    ///
    /// Idempotent, and that matters: it is called from `learn_routes` on **every** conf change
    /// entry a `Ready` carries, including ones this member has already seen after a restart replays
    /// its log. Rebuilding on every replayed entry would tear down the group's connections once per
    /// entry at exactly the moment it was trying to catch up.
    pub fn reconfigure(&self, members: &MemberList) -> Result<()> {
        {
            let held = self
                .members
                .read()
                .map_err(|_| PdError::internal("the placement driver's wiring lock is poisoned"))?;
            if *held == *members {
                return Ok(());
            }
        }
        let replacement = {
            // Inside the runtime, because the delivery tasks are spawned on it and this is called
            // from the driver thread.
            let _guard = self.runtime.enter();
            PdTcpTransport::spawn(self.id, members, self.config)?
        };
        let previous = {
            let mut inner = self
                .inner
                .write()
                .map_err(|_| PdError::internal("the placement driver's wiring lock is poisoned"))?;
            std::mem::replace(&mut *inner, replacement)
        };
        // After the swap, so a message sent during it goes somewhere rather than nowhere. The old
        // transport's own `Drop` aborts its tasks; this only makes the moment explicit.
        previous.shutdown();
        let mut held = self
            .members
            .write()
            .map_err(|_| PdError::internal("the placement driver's wiring lock is poisoned"))?;
        *held = members.clone();
        tracing::info!(
            id = self.id,
            members = members.len(),
            group_id = format_args!("{:#018x}", members.group_id()),
            "the placement driver's group changed; reconnecting"
        );
        Ok(())
    }
}

impl PdTransport for PdWiring {
    fn send(&self, messages: Vec<Message>) {
        let Ok(inner) = self.inner.read() else {
            tracing::error!(
                id = self.id,
                "the placement driver's wiring lock is poisoned"
            );
            return;
        };
        inner.send(messages);
    }

    fn reconfigure(&self, members: &MemberList) {
        if let Err(error) = PdWiring::reconfigure(self, members) {
            // A failure here costs reachability to a member, which Raft will keep retrying and an
            // operator can fix by restarting this one. It is not a reason to stop applying: the log
            // is the truth and this is a cache of where to send it.
            tracing::error!(id = self.id, %error, "could not rewire for the new group");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PdWiring;
    use crate::driver::PdTransport;
    use crate::member::{MemberList, PdMember};
    use esker_proto::TransportConfig;
    use esker_raft::Message;

    fn three() -> MemberList {
        MemberList::new(vec![
            PdMember::new(1, "127.0.0.1:33379"),
            PdMember::new(2, "127.0.0.1:33380"),
            PdMember::new(3, "127.0.0.1:33381"),
        ])
        .unwrap()
    }

    #[tokio::test]
    async fn it_rewires_for_a_member_that_was_not_there() {
        let founded = three().with_group_id(0xABCD);
        let wiring = PdWiring::spawn(1, &founded, TransportConfig::new()).unwrap();
        assert_eq!(wiring.members().len(), 3);

        let grown = founded.with(PdMember::new(4, "127.0.0.1:33382")).unwrap();
        wiring.reconfigure(&grown).unwrap();
        assert_eq!(wiring.members().len(), 4);
        // The group is still the same group: rebuilding must not rename it, or every batch this
        // member sends after a change would be refused by the ones that did not change.
        assert_eq!(wiring.members().group_id(), 0xABCD);

        // And a member that leaves.
        let shrunk = grown.without(2).unwrap();
        wiring.reconfigure(&shrunk).unwrap();
        assert_eq!(wiring.members().len(), 3);
        assert!(!wiring.members().contains(2));
        assert_eq!(wiring.members().group_id(), 0xABCD);
    }

    /// **Idempotent, and that is load-bearing.** `learn_routes` calls this for every conf change
    /// entry a `Ready` carries, and a restart replays every one of them. A rebuild per replayed
    /// entry would tear the group's connections down once per entry, at the moment this member was
    /// trying to catch up.
    #[tokio::test]
    async fn rewiring_for_the_group_it_already_has_changes_nothing() {
        let founded = three().with_group_id(0xABCD);
        let wiring = PdWiring::spawn(1, &founded, TransportConfig::new()).unwrap();
        let before = std::sync::Arc::as_ptr(&*wiring.inner.read().unwrap());
        for _ in 0..8 {
            wiring.reconfigure(&founded).unwrap();
        }
        let after = std::sync::Arc::as_ptr(&*wiring.inner.read().unwrap());
        assert!(
            std::ptr::eq(before, after),
            "rewiring for an unchanged group replaced the transport"
        );
    }

    /// The trait's method is what the driver calls, and it must never fail an apply: the log is the
    /// truth and this is a cache of where to send it.
    #[tokio::test]
    async fn the_trait_method_swallows_what_it_cannot_do() {
        let wiring = PdWiring::spawn(1, &three(), TransportConfig::new()).unwrap();
        // A list with an address nothing can parse never reaches here — `MemberList::new` refuses
        // it — so the reachable failure is the runtime going away, which a test cannot stage.
        // What it can assert is that a send still works after a rewire, which is the thing a
        // swallowed failure must not have broken.
        PdTransport::reconfigure(&*wiring, &three().with_group_id(1));
        wiring.send(vec![Message::TimeoutNow {
            from: 1,
            to: 2,
            term: 1,
        }]);
    }
}
