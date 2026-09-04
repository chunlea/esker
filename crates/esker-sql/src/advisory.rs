//! Advisory locks: the one lock in this node a *client* takes and holds, and the only state a
//! session owns that a transaction cannot roll back.
//!
//! `ActiveRecord::Migrator` wraps every migration in one (`migration.rb:1610`), so this is not a
//! feature the suite uses so much as how it starts: a node that cannot answer
//! `pg_try_advisory_lock` fails at the first migration of a file and takes the file with it.
//!
//! # What the capture settled
//!
//! Every rule below is from `tests/corpus/pg19_advisory_lock.txt`, measured against 19beta1:
//!
//! * **A session lock is not transactional.** One taken inside `BEGIN` is still held after
//!   `ROLLBACK`. It is released by an explicit unlock or by the session ending, and by nothing
//!   else — which is why this table is not in the transaction's write set and not in the store.
//! * **The one- and two-argument forms are different key spaces**, distinguished by `objsubid`
//!   (1 for the `bigint`, 2 for the pair), and a session may hold both at once.
//! * **A lock is re-entrant and counted**: two takes need two unlocks, and the third answers
//!   `false`.
//! * **Releasing one you do not hold is `false` with a `WARNING`**, never an error.
//! * `Exclusive` conflicts with everything; `Share` conflicts only with `Exclusive`.
//!
//! # Where it lives, and what that costs
//!
//! One [`Locks`] per **node**, shared by every session that node serves — `Sessions` in
//! `bin/esker-sql.rs` holds it beside the catalog cache and hands each executor an `Arc` of it.
//! An executor built without one gets a private table, which is right for a single-session test
//! and is what keeps the corpus replay honest.
//!
//! **A real server's advisory locks are cluster-wide and this node's are node-wide**, which is a
//! declared divergence (`docs/plans/phase-9-rails.md` §6): two `esker-sql` processes in front of
//! one store do not see each other's. For the thing the suite uses them for — one migrator process
//! serializing against itself — node-wide is the whole requirement, and making it more would mean
//! putting a lock in the store, where a crashed session's lock would outlive it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Which key space a lock is in — PostgreSQL's `pg_locks.objsubid`, and the reason
/// `pg_try_advisory_lock(42)` and `pg_try_advisory_lock(42, 7)` are two locks rather than one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Space {
    /// The `bigint` form. `objsubid` 1.
    Whole,
    /// The `(int4, int4)` form. `objsubid` 2.
    Pair,
}

impl Space {
    /// What `pg_locks.objsubid` reports.
    #[must_use]
    pub fn objsubid(self) -> i16 {
        match self {
            Space::Whole => 1,
            Space::Pair => 2,
        }
    }
}

/// Exclusive or shared. `pg_locks.mode` prints these words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `pg_advisory_lock` and friends. Conflicts with everything.
    Exclusive,
    /// `pg_advisory_lock_shared` and friends. Conflicts only with [`Mode::Exclusive`].
    Shared,
}

impl Mode {
    /// The word `pg_locks.mode` uses, and the one the `WARNING` for releasing an unheld lock names.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Mode::Exclusive => "ExclusiveLock",
            Mode::Shared => "ShareLock",
        }
    }
}

/// One lock's identity: the key and which space it is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Key {
    /// The 64-bit key. The pair form packs its two `int4`s here, high half first, so one map holds
    /// both spaces and `pg_locks` can split it back apart the way a real server does.
    pub key: i64,
    /// Which space, which is what keeps `42` and `(42, 7)` apart even where the packing collides.
    pub space: Space,
}

impl Key {
    /// The `bigint` form.
    #[must_use]
    pub fn whole(key: i64) -> Self {
        Key {
            key,
            space: Space::Whole,
        }
    }

    /// The `(int4, int4)` form, packed high half first — the same order
    /// `(classid::bigint << 32) | objid::bigint` reads it back in, which is the expression
    /// `connection_test.rb` uses.
    #[must_use]
    pub fn pair(high: i32, low: i32) -> Self {
        // The low half is masked rather than sign-extended: `pair(1, -1)` must pack as
        // `0x0000_0001_ffff_ffff`, which is what `(classid << 32) | objid` reads back.
        let packed = (i64::from(high) << 32) | (i64::from(low) & 0xffff_ffff);
        Key {
            key: packed,
            space: Space::Pair,
        }
    }

    /// `pg_locks.classid`: the high half, as an unsigned 32-bit the way an `oid` prints.
    #[must_use]
    pub fn classid(self) -> u32 {
        u32::try_from((self.key >> 32) & 0xffff_ffff).unwrap_or_default()
    }

    /// `pg_locks.objid`: the low half.
    #[must_use]
    pub fn objid(self) -> u32 {
        u32::try_from(self.key & 0xffff_ffff).unwrap_or_default()
    }
}

/// Who holds a lock. One per session, handed out by [`Locks::session`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Session(u64);

/// One row of what a session holds: the mode, and **how many times**.
#[derive(Debug, Clone, Copy)]
struct Held {
    mode: Mode,
    /// Re-entrancy is counted, not a flag: two takes need two unlocks, measured.
    depth: u32,
}

/// One row of `pg_locks` for an advisory lock.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// The lock.
    pub key: Key,
    /// Its mode.
    pub mode: Mode,
    /// The session holding it, which `pg_locks.pid` reports.
    pub session: Session,
}

impl Session {
    /// The number `pg_locks.pid` reports for this session.
    ///
    /// **Not a process id**, and it cannot be: every session here is a task in one process, where
    /// a real server gives each its own backend. What the one test that reads it needs is that
    /// sessions are *told apart*, which a counter does.
    #[must_use]
    pub fn pid(self) -> i32 {
        i32::try_from(self.0).unwrap_or(i32::MAX)
    }
}

/// A node's advisory locks.
///
/// The whole table is one mutex: a lock take is a hash lookup and there is no waiting inside it —
/// `pg_try_advisory_lock` answers `false` rather than blocking, and the blocking form is refused by
/// name (nothing `ActiveRecord` sends is blocking).
#[derive(Debug, Default)]
pub struct Locks {
    held: Mutex<HashMap<Key, Vec<(Session, Held)>>>,
    next_session: AtomicU64,
}

impl Locks {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Locks::default()
    }

    /// A fresh session identity. Never reused, so a released lock cannot be mistaken for a new
    /// session's.
    pub fn session(&self) -> Session {
        Session(self.next_session.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// `pg_try_advisory_lock` and its shared form: takes the lock or answers `false` at once.
    ///
    /// **A session never conflicts with itself, and every other session does.** Both halves are
    /// measured, and getting either wrong is a wrong answer rather than a missing one:
    ///
    /// * Alone, `pg_try_advisory_lock_shared(k)` then `pg_try_advisory_lock(k)` both answer `t`,
    ///   and `pg_locks` then shows **two rows** — `ShareLock` and `ExclusiveLock`. It is not an
    ///   upgrade: the session holds both, and each needs its own unlock.
    /// * With *another* session holding a share, that same second call answers `f`. So the
    ///   conflict test is over other holders only, and a session's own holds never block it.
    ///
    /// A holder is therefore `(session, mode)` rather than a session, which is also what makes
    /// [`Locks::unlock`] release the right one. Reading it as re-entrancy per *session* is the
    /// mistake this shape exists to refuse, and it answers `true` where a real server says `false`.
    pub fn try_lock(&self, session: Session, key: Key, mode: Mode) -> bool {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let holders = held.entry(key).or_default();
        let conflict = holders.iter().any(|(who, entry)| {
            *who != session && (entry.mode == Mode::Exclusive || mode == Mode::Exclusive)
        });
        if conflict {
            // Nothing was added, so an empty vector left behind would be a lock nobody holds.
            if holders.is_empty() {
                held.remove(&key);
            }
            return false;
        }
        if let Some((_, entry)) = holders
            .iter_mut()
            .find(|(who, entry)| *who == session && entry.mode == mode)
        {
            entry.depth += 1;
        } else {
            holders.push((session, Held { mode, depth: 1 }));
        }
        true
    }

    /// `pg_advisory_unlock` and its shared form: releases **one** hold.
    ///
    /// `false` when this session does not hold it in that mode — a key nobody took, one already
    /// released, and one another session holds are all the same answer, and the caller turns it
    /// into PostgreSQL's `WARNING`.
    pub fn unlock(&self, session: Session, key: Key, mode: Mode) -> bool {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(holders) = held.get_mut(&key) else {
            return false;
        };
        let Some(at) = holders
            .iter()
            .position(|(who, entry)| *who == session && entry.mode == mode)
        else {
            return false;
        };
        holders[at].1.depth -= 1;
        if holders[at].1.depth == 0 {
            holders.remove(at);
        }
        if holders.is_empty() {
            held.remove(&key);
        }
        true
    }

    /// `pg_advisory_unlock_all()`: everything this session holds, whatever the depth.
    pub fn unlock_all(&self, session: Session) {
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.retain(|_, holders| {
            holders.retain(|(who, _)| *who != session);
            !holders.is_empty()
        });
    }

    /// Every advisory lock on this node, for `pg_locks`.
    ///
    /// Sorted so a `SELECT` without an `ORDER BY` is still repeatable — the rest of this crate's
    /// catalog views make the same promise for the same reason.
    pub fn rows(&self) -> Vec<Row> {
        let held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<Row> = held
            .iter()
            .flat_map(|(key, holders)| {
                holders.iter().map(|(session, entry)| Row {
                    key: *key,
                    mode: entry.mode,
                    session: *session,
                })
            })
            .collect();
        rows.sort_by_key(|row| (row.key, row.session, row.mode.name()));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::{Key, Locks, Mode, Space};

    /// The counting the capture pins: two takes, two unlocks, and the third is `false`.
    #[test]
    fn a_lock_is_re_entrant_and_counted() {
        let locks = Locks::new();
        let session = locks.session();
        let key = Key::whole(7);
        assert!(locks.try_lock(session, key, Mode::Exclusive));
        assert!(locks.try_lock(session, key, Mode::Exclusive));
        // One row however many times it is held, which is what `count(*)` over `pg_locks` shows.
        assert_eq!(locks.rows().len(), 1);
        assert!(locks.unlock(session, key, Mode::Exclusive));
        assert!(locks.unlock(session, key, Mode::Exclusive));
        assert!(!locks.unlock(session, key, Mode::Exclusive));
        assert!(locks.rows().is_empty());
    }

    /// Exclusive conflicts with everything, shared only with exclusive — and a session never
    /// blocks itself.
    #[test]
    fn the_conflict_table_is_postgresql_s() {
        let locks = Locks::new();
        let (a, b) = (locks.session(), locks.session());
        let key = Key::whole(9);

        assert!(locks.try_lock(a, key, Mode::Exclusive));
        assert!(!locks.try_lock(b, key, Mode::Exclusive));
        assert!(!locks.try_lock(b, key, Mode::Shared));
        assert!(
            locks.try_lock(a, key, Mode::Exclusive),
            "a session holding a lock takes it again rather than deadlocking with itself"
        );
        locks.unlock_all(a);

        assert!(locks.try_lock(a, key, Mode::Shared));
        assert!(locks.try_lock(b, key, Mode::Shared), "two shares agree");
        assert!(!locks.try_lock(locks.session(), key, Mode::Exclusive));

        // **A session holding a share cannot take an exclusive while *another* holds one** —
        // measured `f`, and the case that reads as re-entrancy and is not.
        assert!(
            !locks.try_lock(b, key, Mode::Exclusive),
            "b's own share does not excuse it from a's"
        );
        locks.unlock_all(b);
        // Alone, the same pair succeeds and is **two holds**, not an upgrade: a real server's
        // `pg_locks` shows a `ShareLock` and an `ExclusiveLock` for the one session, and each
        // needs its own unlock.
        assert!(locks.try_lock(a, key, Mode::Exclusive));
        let modes: Vec<&str> = locks.rows().iter().map(|row| row.mode.name()).collect();
        assert_eq!(modes, ["ExclusiveLock", "ShareLock"]);
        assert!(locks.unlock(a, key, Mode::Exclusive));
        assert!(locks.unlock(a, key, Mode::Shared));
        assert!(locks.rows().is_empty());
    }

    /// **The two forms are two key spaces**, and the packing is what `pg_locks` splits back apart.
    #[test]
    fn the_pair_form_is_a_different_lock_from_the_whole_one() {
        let locks = Locks::new();
        let session = locks.session();
        assert!(locks.try_lock(session, Key::whole(42), Mode::Exclusive));
        assert!(
            locks.try_lock(session, Key::pair(42, 7), Mode::Exclusive),
            "the pair form is not the same lock as the bigint 42"
        );
        assert_eq!(locks.rows().len(), 2);

        // `(classid << 32) | objid` is the expression `connection_test.rb` rebuilds the key with.
        let whole = Key::whole(5_295_901_941_911_233_559);
        assert_eq!(whole.classid(), 1_233_048_257);
        assert_eq!(whole.objid(), 3_706_430_487);
        assert_eq!(
            (i64::from(whole.classid()) << 32) | i64::from(whole.objid()),
            5_295_901_941_911_233_559
        );
        assert_eq!(Space::Whole.objsubid(), 1);
        assert_eq!(Space::Pair.objsubid(), 2);

        // A negative low half must not sign-extend into the high one.
        assert_eq!(Key::pair(1, -1).key, (1i64 << 32) | 0xffff_ffff);
    }

    /// One session's locks go together and nobody else's move.
    #[test]
    fn unlock_all_takes_only_this_session_s() {
        let locks = Locks::new();
        let (a, b) = (locks.session(), locks.session());
        assert!(locks.try_lock(a, Key::whole(1), Mode::Exclusive));
        assert!(locks.try_lock(a, Key::whole(2), Mode::Exclusive));
        assert!(locks.try_lock(b, Key::whole(3), Mode::Exclusive));
        locks.unlock_all(a);
        let rows = locks.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key, Key::whole(3));
    }

    /// A lock another session holds is not yours to release, and saying so is `false` rather than
    /// an error.
    #[test]
    fn a_session_releases_only_its_own() {
        let locks = Locks::new();
        let (a, b) = (locks.session(), locks.session());
        let key = Key::whole(11);
        assert!(locks.try_lock(a, key, Mode::Exclusive));
        assert!(!locks.unlock(b, key, Mode::Exclusive));
        assert_eq!(locks.rows().len(), 1, "and it is still held");
        // Nor in the wrong mode.
        assert!(!locks.unlock(a, key, Mode::Shared));
        assert!(locks.unlock(a, key, Mode::Exclusive));
    }
}
