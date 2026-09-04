//! Who the placement drivers are, and what tells one group's traffic from another's.
//!
//! PD's membership is **configuration, not state**
//! ([ADR 0059](../../../docs/adr/0059-pd-is-a-raft-group.md)): the same list on every member,
//! given on the command line, never changed at run time. Dynamic membership is a phase of its
//! own, and what it would have to move is written down at the bottom of this file.
//!
//! # The group id
//!
//! [ADR 0011](../../../docs/adr/0011-pd-service-and-the-cluster-id.md) put a cluster id on every
//! PD request for a mistake people actually make: a stale `--pd` flag pointing at another
//! cluster, whose symptom without a check is not an error but an *answer*. PD's Raft group has the
//! same exposure and cannot use the same guard, because the group has to elect a leader **before**
//! `Bootstrap` — itself a log entry — has minted anything.
//!
//! So a group is identified by what it is: the set of its members. [`MemberList::group_id`] mixes
//! the sorted `id@address` pairs, a member refuses a batch that does not carry its own, and two
//! placement drivers from different clusters pointed at each other by a stale flag cannot form one
//! group and replicate one cluster's routing table over the other's.
//!
//! Sorted, so listing the endpoints in a different order on different members is the same group.
//! Changed by adding a member — which is correct while membership is static, and is exactly the
//! line that has to move when it stops being.

use std::fmt;
use std::net::SocketAddr;

use esker_raft::NodeId;

use crate::error::{PdError, Result};

/// Members a placement-driver group may have.
///
/// **Three is the shape this is built and tested for**, and five is what the arithmetic of a
/// replacement needs: `add`-before-`remove` puts the group transiently at four
/// ([ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)
/// §11.4), and refusing that would refuse the recovery path itself. A deployment that settles above
/// three is outside what this phase measured.
pub const MAX_MEMBERS: usize = 5;

/// One placement driver in the group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdMember {
    /// Its Raft node id. Unique within the group and never zero, which `esker-raft` reserves for
    /// "no node".
    pub id: NodeId,
    /// Where it listens, as written on the command line. Empty for a group of one, which has
    /// nobody to reach.
    pub address: String,
}

impl PdMember {
    /// A member at an address.
    pub fn new(id: NodeId, address: impl Into<String>) -> Self {
        Self {
            id,
            address: address.into(),
        }
    }
}

impl fmt::Display for PdMember {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}@{}", self.id, self.address)
    }
}

/// The group, as this member was configured to see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberList {
    /// The members, sorted by id. Sorting is what makes the group id independent of the order the
    /// endpoints were written in.
    members: Vec<PdMember>,
    /// The group's **recorded** id, once it has one.
    ///
    /// `None` on a group being founded, which derives its id from the list below and then writes
    /// the answer down. From then on this is what [`MemberList::group_id`] answers, and adding a
    /// member does not change it — which is the whole of what
    /// [ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)
    /// had to change before membership could move at all.
    ///
    /// It is also what lets `crate::transport` stay untouched: that module stamps
    /// `members.group_id()` onto every batch, and with only a derivation to read it would be
    /// stamping an id that moved the moment a member was added.
    group_id: Option<u64>,
}

impl MemberList {
    /// The list a lone placement driver has: itself, reachable by nobody.
    #[must_use]
    pub fn alone(id: NodeId) -> Self {
        Self {
            members: vec![PdMember::new(id, String::new())],
            group_id: None,
        }
    }

    /// The list a member joining an existing group holds: its members, and **its** id.
    ///
    /// The id comes from the group rather than from this list, because the two disagree the moment
    /// a member is added and the group's own answer is the one that is right.
    pub fn joining(members: Vec<PdMember>, group_id: u64) -> Result<Self> {
        Ok(Self {
            group_id: Some(group_id),
            ..Self::new(members)?
        })
    }

    /// This list with `group_id` recorded on it.
    #[must_use]
    pub fn with_group_id(self, group_id: u64) -> Self {
        Self {
            group_id: Some(group_id),
            ..self
        }
    }

    /// The same members, with `member` added or its address replaced.
    pub fn with(&self, member: PdMember) -> Result<Self> {
        let mut members: Vec<PdMember> = self
            .members
            .iter()
            .filter(|held| held.id != member.id)
            .cloned()
            .collect();
        members.push(member);
        Ok(Self {
            group_id: self.group_id,
            ..Self::new(members)?
        })
    }

    /// The same members without `id`. Removing one that is not there changes nothing.
    pub fn without(&self, id: NodeId) -> Result<Self> {
        let members: Vec<PdMember> = self
            .members
            .iter()
            .filter(|held| held.id != id)
            .cloned()
            .collect();
        Ok(Self {
            group_id: self.group_id,
            ..Self::new(members)?
        })
    }

    /// Builds the list, refusing one that cannot work.
    ///
    /// Every check here is of a mistake that would otherwise surface as a cluster that does not
    /// elect, or — worse — as two groups that half-agree.
    pub fn new(members: Vec<PdMember>) -> Result<Self> {
        if members.is_empty() {
            return Err(PdError::invalid("a placement-driver group has no members"));
        }
        if members.len() > MAX_MEMBERS {
            return Err(PdError::invalid(format!(
                "a placement-driver group of {} is more than the {MAX_MEMBERS} this build \
                 supports",
                members.len()
            )));
        }
        let mut members = members;
        members.sort_by_key(|member| member.id);
        for pair in members.windows(2) {
            if pair[0].id == pair[1].id {
                return Err(PdError::invalid(format!(
                    "two placement drivers share member id {}",
                    pair[0].id
                )));
            }
        }
        for member in &members {
            if member.id == 0 {
                return Err(PdError::invalid("member id 0 is reserved for 'no member'"));
            }
            if members.len() > 1 {
                if member.address.is_empty() {
                    return Err(PdError::invalid(format!(
                        "placement driver {} has no address, and a group of {} needs one for each",
                        member.id,
                        members.len()
                    )));
                }
                member.address.parse::<SocketAddr>().map_err(|error| {
                    PdError::invalid(format!(
                        "placement driver {}'s address `{}` is not an address: {error}",
                        member.id, member.address
                    ))
                })?;
            }
        }
        Ok(Self {
            members,
            group_id: None,
        })
    }

    /// The members, by id.
    #[must_use]
    pub fn members(&self) -> &[PdMember] {
        &self.members
    }

    /// How many there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the group is a group of one, which needs no election and no transport.
    #[must_use]
    pub fn is_alone(&self) -> bool {
        self.members.len() == 1
    }

    /// Never true: a list with no members is refused at construction.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// The ids, for a `esker-raft` configuration.
    #[must_use]
    pub fn ids(&self) -> Vec<NodeId> {
        self.members.iter().map(|member| member.id).collect()
    }

    /// Where a member is, or `None` for one this list does not name.
    #[must_use]
    pub fn address_of(&self, id: NodeId) -> Option<&str> {
        self.members
            .iter()
            .find(|member| member.id == id)
            .map(|member| member.address.as_str())
    }

    /// Whether `id` is in the group.
    #[must_use]
    pub fn contains(&self, id: NodeId) -> bool {
        self.members.iter().any(|member| member.id == id)
    }

    /// What identifies this group's traffic. See the module documentation.
    ///
    /// **Two answers, and the type says which.** A group that has one recorded gives that; a group
    /// being founded derives one from its members. A group that has never changed membership
    /// cannot tell the difference — the recorded id *is* the derivation of the founding list — and
    /// that is what makes the upgrade to
    /// [ADR 0060](../../../docs/adr/0060-a-placement-driver-joins-a-group-it-is-told-the-name-of.md)
    /// invisible to every deployment that exists.
    #[must_use]
    pub fn group_id(&self) -> u64 {
        self.group_id.unwrap_or_else(|| self.derived_group_id())
    }

    /// The id recorded on this list, if any. `None` means it would be derived.
    #[must_use]
    pub fn recorded_group_id(&self) -> Option<u64> {
        self.group_id
    }

    /// The id this membership derives, whatever is recorded on it.
    ///
    /// Called **once** in the life of a group, to mint what is then written down. Calling it later
    /// answers a different question — "what would a group founded with these members be called" —
    /// and the answer moves as the membership does, which is exactly why it is not what
    /// [`MemberList::group_id`] returns.
    ///
    /// A group of one gets an id too, and it is a real one: the day somebody points a second
    /// process at a single-member PD's port is the day it matters, and an exemption is a hole
    /// nobody would remember to close.
    #[must_use]
    pub fn derived_group_id(&self) -> u64 {
        let mut mixed = esker_base::hash::mix64(self.members.len() as u64);
        for member in &self.members {
            mixed = esker_base::hash::mix64(mixed ^ esker_base::hash::mix64(member.id));
            mixed = esker_base::hash::mix64(
                mixed ^ esker_base::hash::hash64(member.address.as_bytes()),
            );
        }
        // Zero is reserved everywhere in this codebase's formats, so a zeroed field must not read
        // as a valid group.
        if mixed == 0 { 1 } else { mixed }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_MEMBERS, MemberList, PdMember};

    fn three() -> Vec<PdMember> {
        vec![
            PdMember::new(1, "127.0.0.1:2379"),
            PdMember::new(2, "127.0.0.1:2380"),
            PdMember::new(3, "127.0.0.1:2381"),
        ]
    }

    /// The whole point of sorting: an operator who writes the endpoints in a different order on
    /// each machine still gets one group, not three.
    #[test]
    fn the_group_id_does_not_depend_on_the_order_the_endpoints_were_written_in() {
        let forwards = MemberList::new(three()).unwrap();
        let mut backwards = three();
        backwards.reverse();
        let backwards = MemberList::new(backwards).unwrap();
        assert_eq!(forwards.group_id(), backwards.group_id());
        assert_eq!(forwards.members(), backwards.members());
    }

    /// And the point of having one at all: a different set of members is a different group, so
    /// two clusters' placement drivers cannot merge into one.
    #[test]
    fn a_different_membership_is_a_different_group() {
        let group = MemberList::new(three()).unwrap();
        let mut other = three();
        other[2] = PdMember::new(3, "10.0.0.9:2381");
        let other = MemberList::new(other).unwrap();
        assert_ne!(group.group_id(), other.group_id(), "an address moved");

        let smaller = MemberList::new(three()[..2].to_vec()).unwrap();
        assert_ne!(group.group_id(), smaller.group_id(), "a member left");

        let lone = MemberList::alone(1);
        assert_ne!(lone.group_id(), group.group_id());
        assert_ne!(lone.group_id(), 0, "zero is reserved");
    }

    /// Each of these would otherwise surface as a group that does not elect, and be diagnosed by
    /// reading three log files side by side.
    #[test]
    fn a_membership_that_cannot_work_is_refused_at_the_boundary() {
        assert!(MemberList::new(Vec::new()).is_err(), "empty");
        assert!(
            MemberList::new(vec![PdMember::new(0, "127.0.0.1:2379")]).is_err(),
            "id zero"
        );
        assert!(
            MemberList::new(vec![
                PdMember::new(1, "127.0.0.1:2379"),
                PdMember::new(1, "127.0.0.1:2380"),
            ])
            .is_err(),
            "duplicate id"
        );
        assert!(
            MemberList::new(vec![
                PdMember::new(1, String::new()),
                PdMember::new(2, "127.0.0.1:2380"),
            ])
            .is_err(),
            "a group of two needs an address for each"
        );
        assert!(
            MemberList::new(vec![
                PdMember::new(1, "not-an-address"),
                PdMember::new(2, "127.0.0.1:2380"),
            ])
            .is_err(),
            "an address that is not one"
        );
        let mut past = three();
        for id in 4..=(MAX_MEMBERS as u64 + 1) {
            past.push(PdMember::new(id, format!("127.0.0.1:24{id:02}")));
        }
        assert!(MemberList::new(past).is_err(), "more than {MAX_MEMBERS}");
    }

    /// **The change that made dynamic membership possible.** A derived id moves when a member is
    /// added; a recorded one does not, which is why the recorded one is what the transport stamps.
    #[test]
    fn a_recorded_group_id_survives_a_membership_change_and_a_derived_one_does_not() {
        let founded = MemberList::new(three()).unwrap();
        let name = founded.derived_group_id();
        let founded = founded.with_group_id(name);

        let grown = founded.with(PdMember::new(4, "127.0.0.1:2382")).unwrap();
        assert_eq!(grown.len(), 4);
        assert_eq!(grown.group_id(), name, "adding a member renamed the group");
        assert_ne!(
            grown.derived_group_id(),
            name,
            "the derivation is supposed to move — if it did not, this test proves nothing"
        );

        let shrunk = grown.without(2).unwrap();
        assert_eq!(shrunk.len(), 3);
        assert_eq!(
            shrunk.group_id(),
            name,
            "removing a member renamed the group"
        );
        assert!(!shrunk.contains(2));
        assert_eq!(shrunk.address_of(4), Some("127.0.0.1:2382"));
    }

    /// A group being founded has no recorded id and derives one; a member joining an existing group
    /// is **told** it, because the two disagree the moment a member is added and the group's own
    /// answer is the one that is right.
    #[test]
    fn a_founding_group_derives_its_name_and_a_joining_member_is_told_it() {
        let founding = MemberList::new(three()).unwrap();
        assert_eq!(founding.recorded_group_id(), None);
        assert_eq!(founding.group_id(), founding.derived_group_id());

        let joining = MemberList::joining(three(), 0xFEED).unwrap();
        assert_eq!(joining.recorded_group_id(), Some(0xFEED));
        assert_eq!(joining.group_id(), 0xFEED);
        assert_ne!(joining.derived_group_id(), 0xFEED);
    }

    /// Replacing an address is `with` on an id that is already there, not a second entry for it.
    #[test]
    fn adding_a_member_that_is_already_there_replaces_its_address() {
        let group = MemberList::new(three()).unwrap();
        let moved = group.with(PdMember::new(2, "10.0.0.2:2380")).unwrap();
        assert_eq!(moved.len(), 3);
        assert_eq!(moved.address_of(2), Some("10.0.0.2:2380"));
    }

    /// The transient fourth member an `add`-before-`remove` needs must be allowed, or the recovery
    /// path is refused by the type that exists to describe it.
    #[test]
    fn a_replacement_may_take_the_group_to_four() {
        let mut four = three();
        four.push(PdMember::new(4, "127.0.0.1:2382"));
        assert!(MemberList::new(four.clone()).is_ok());
        four.push(PdMember::new(5, "127.0.0.1:2383"));
        assert!(MemberList::new(four.clone()).is_ok(), "five is the cap");
        four.push(PdMember::new(6, "127.0.0.1:2384"));
        assert!(MemberList::new(four).is_err(), "six is past it");
    }

    /// A group of one is the 4a shape, and it needs no address because it has nobody to reach.
    #[test]
    fn a_group_of_one_needs_no_address() {
        let lone = MemberList::alone(1);
        assert!(lone.is_alone());
        assert_eq!(lone.ids(), vec![1]);
        assert_eq!(lone.address_of(1), Some(""));
        assert!(lone.contains(1));
        assert!(!lone.contains(2));
    }
}
