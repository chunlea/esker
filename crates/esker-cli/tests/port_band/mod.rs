//! One port allocator for every test that starts a real process.
//!
//! # The bug this exists to end
//!
//! Twelve of these files picked their ports out of a **fixed band** — 30,100 for `cluster_start`,
//! 31,100 for `cluster_pd_group`, 34,100 for `cluster_pd_member_change`, 41,000 for both
//! `pgwire_cluster` and `columnar_cluster` — by binding each port of a run, checking they all
//! bound, dropping the listeners and returning the base. Each carried a comment saying a private
//! band made the release-then-bind race harmless *between test binaries*, and that much is true.
//! **It is not harmless within one.** `nextest` runs every test in its own process, so two tests
//! of the same file are two processes walking the same band from the same end, and both start at
//! its first port. Whenever one is between its release and its child's `bind`, the other picks
//! exactly the same run:
//!
//! ```text
//! esker server: listening on 127.0.0.1:34100: io error: Address already in use (os error 98)
//! esker cluster: node 1 exited with exit status: 1
//! ```
//!
//! That line failed four gates in `esker-coord/`'s archive — two of this lane's and two of
//! another's — always in the first ports of a band, and it reads exactly like a timing flake
//! because a loaded machine widens the window. The two files that shared the 41,000 band needed no
//! window at all.
//!
//! # This is not a new idea — it is `cluster_chaos`'s, applied everywhere
//!
//! `tests/cluster_chaos.rs` and `tests/tier_acceptance.rs` had already found it, fixed it, and
//! written the test that names it (`a_reserved_port_run_is_held_and_never_handed_out_twice`). The
//! reasoning below is theirs; what is new here is that the other twelve files share it instead of
//! each keeping a band. Three parts, and the third is the one a reader is most likely to drop:
//!
//! * the run is **held** — the listeners live in the returned [`Ports`] and are released on the
//!   line before the child is spawned, so the window is one statement rather than a whole setup,
//!   and a second caller running at the same instant cannot be handed the same run;
//! * the scan **starts at a slot derived from the clock and the pid** rather than at the bottom of
//!   the range, so two processes do not both begin at the first slot;
//! * and the range sits **below 32,768**, under the ephemeral ports the kernel hands to outgoing
//!   connections — a band inside that range can be taken by any of the suite's thousands of client
//!   sockets while nothing is holding it, which binding `127.0.0.1:0` and releasing does not avoid
//!   either.
//!
//! The remaining window, from the release to the child's own `bind`, cannot be closed from here:
//! `cluster start` numbers its nodes from one base port so the run has to be consecutive, and the
//! servers are separate processes, so the sockets cannot be passed to them the way an in-process
//! harness passes a listener. A caller that can retry should; `cluster_chaos` does.

#![allow(dead_code, unreachable_pub)]

use std::net::TcpListener;

/// The lowest port a run may start at. Below the ephemeral range, which is the point: an ephemeral
/// port is one the kernel may hand to somebody's outgoing connection the moment it is released.
const LOW: u16 = 20_000;

/// One past the highest port a run may occupy. 32,768 is where Linux's ephemeral range begins.
const HIGH: u16 = 32_768;

/// A run of consecutive ports, held bound until the caller gives them to a child.
pub struct Ports {
    base: u16,
    /// Never read: being bound *is* what it does. Dropped when the reservation is consumed.
    held: Vec<TcpListener>,
}

impl Ports {
    /// The first port of the run, while the run is still held.
    pub fn base(&self) -> u16 {
        self.base
    }

    /// The `step`th port of the run.
    pub fn at(&self, step: u16) -> u16 {
        self.base + step
    }

    /// Releases the run and answers its base — **call this immediately before spawning**.
    ///
    /// Named for what it does to the reservation rather than for what it returns, because the
    /// release is the part that matters: everything before it is time during which no other
    /// process can be given these ports.
    pub fn into_base(self) -> u16 {
        self.base
    }

    /// The base and the listeners, for a caller that holds the run across a longer setup and
    /// drops it on the line before spawning — which is what every caller should do where the
    /// shape of the test allows it.
    pub fn into_parts(self) -> (u16, Vec<TcpListener>) {
        (self.base, self.held)
    }
}

/// Reserves `span` consecutive free ports, held until the returned value is consumed.
pub fn reserve(span: u16) -> Ports {
    assert!(span > 0, "a run of no ports is not a run");
    let slots = (HIGH - LOW) / span;
    assert!(slots > 0, "a run of {span} ports does not fit the band");
    // No `rand` here (`CLAUDE.md`'s dependency policy), and none is needed: the clock and the pid
    // are enough to keep two processes from starting at the same slot.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let start = (nanos ^ std::process::id()) % u32::from(slots);
    for step in 0..slots {
        let slot = (start + u32::from(step)) % u32::from(slots);
        let Some(base) = u16::try_from(slot)
            .ok()
            .and_then(|slot| slot.checked_mul(span))
            .and_then(|offset| LOW.checked_add(offset))
        else {
            continue;
        };
        let held: Vec<TcpListener> = (0..span)
            .filter_map(|at| TcpListener::bind(("127.0.0.1", base.checked_add(at)?)).ok())
            .collect();
        if held.len() == usize::from(span) {
            return Ports { base, held };
        }
    }
    panic!("no run of {span} consecutive free ports between {LOW} and {HIGH}");
}

/// One free port, held until the caller spawns the child that binds it.
pub fn reserve_one() -> Ports {
    reserve(1)
}
