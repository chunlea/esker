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
/// Three is the shape this is built and tested for. Five would work and nothing here is written
/// for it, so it is refused rather than silently untested.
pub const MAX_MEMBERS: usize = 3;

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
}

impl MemberList {
    /// The list a lone placement driver has: itself, reachable by nobody.
    #[must_use]
    pub fn alone(id: NodeId) -> Self {
        Self {
            members: vec![PdMember::new(id, String::new())],
        }
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
        Ok(Self { members })
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
    /// A group of one gets a group id too, and it is a real one: the day somebody points a second
    /// process at a single-member PD's port is the day it matters, and an exemption is a hole
    /// nobody would remember to close.
    #[must_use]
    pub fn group_id(&self) -> u64 {
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
        let mut four = three();
        four.push(PdMember::new(4, "127.0.0.1:2382"));
        assert!(MemberList::new(four).is_err(), "more than {MAX_MEMBERS}");
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
