//! Garbage collection: which MVCC versions have outlived their usefulness
//! (`docs/txn-spec.md` §7, [ADR 0021](../../../docs/adr/0021-time-machine.md)).
//!
//! # The safepoint is a retention policy, not a number
//!
//! PD publishes one safepoint and it is a **floor**: no store may collect above it, because PD
//! is the only thing that knows the oldest active read. What a *table* keeps is that floor
//! adjusted by its own retention window, which is the same number as how far back a reader may
//! travel — so the knob is retention and the collector takes a policy rather than a timestamp.
//!
//! ```text
//! effective_safepoint(table) = min(published, published + ((default_ms - table_ms) << 18))
//! ```
//!
//! An override may make a table keep **more**, never less. A table with a longer retention has
//! its safepoint pushed back by the difference and works entirely store-side; one with a
//! shorter retention gets no benefit yet, because the `min` discards it — that needs PD to
//! publish the cluster's *smallest* retention, which ADR 0021 names as the protocol addition to
//! make later.
//!
//! Two traps the ADR names and this module must not fall into: `RETENTION_FOREVER` is a
//! **sentinel** and not a duration — subtracting it underflows, and the rule is "collect
//! nothing for this table" — and a table with no override is not "retention zero", it is the
//! cluster default, which is the difference between an absent key and a key holding zero.
//!
//! # What the collector keeps
//!
//! Below a key's effective safepoint: the **newest version**, because that is what a read at
//! the safepoint returns and dropping it would make an existing key vanish. Everything older
//! goes.
//!
//! **Version** is `Kind::is_a_version` and not "whatever record is newest" (#78). A `Kind::Lock`
//! record — a committed `Op::Check`, which is what a SERIALIZABLE transaction and a
//! `SELECT … FOR UPDATE` leave on a key they only *read* — sits above the version it validated
//! and is **not** one: the read side steps past it looking for a version
//! (`esker_txn::percolator::newest_version_at`). A collector that let one stand in for the newest
//! would keep the lock, drop the `Put` under it, and leave the key reading as absent — which is
//! exactly how run 127 attempt 4 lost five catalog table records while the names pointing at them
//! survived. So a lock record goes on its own `commit_ts` and never claims the slot.
//!
//! A **rollback marker** is the other non-version, and it lives until the safepoint passes its
//! `start_ts`, because below that there can still be an in-flight `Prewrite` that the marker is
//! the only thing stopping (`docs/txn-spec.md` §7).
//!
//! # What it does **not** collect, measured rather than intended (#60)
//!
//! This paragraph used to say "and so does its `default` entry". It does not: nothing in this
//! module writes to or deletes from the `default` family — every `cf::DEFAULT` below reads
//! retention configuration. A value longer than `SHORT_VALUE_MAX_LEN` is stored there at prewrite,
//! keyed by `(user_key, start_ts)`, and it stays after the `write` record naming it is collected.
//!
//! Eight keys, twelve versions, 512-byte values, before and after a safepoint above all of them:
//!
//! ```text
//!      write    96 -> 8       default   96 -> 96       lock   192 -> 192
//! ```
//!
//! **The bytes are precisely what is not reclaimed**, since a version record is tens of bytes and
//! a value is as large as the user made it. The `lock` family is a separate case with its own
//! rule, in
//! [ADR 0110](../../../docs/adr/0110-who-publishes-the-garbage-collection-safepoint.md).
//!
//! A filter over `write` cannot fix this alone: deciding whether a `default` entry is still
//! referenced is a question about **another column family**, and a compaction filter sees one.
//! `docs/plans/debts-v1.1.md` §1 carries #60 with the two routes out.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};

use esker_engine::compaction::{CompactionFilter, FilterDecision};
use esker_engine::{Db, ReadOptions, cf};
use esker_keys::prefix::{self, TablePart};
use esker_proto::ProtoError;
use esker_txn::codec::{Kind, WriteRecord};
use esker_txn::key;

use crate::error::engine_to_proto;

/// A retention of `u64::MAX` means **keep everything, for ever**.
///
/// A sentinel and not a duration: subtracting it from a safepoint underflows, and the rule it
/// stands for is "collect nothing for this table" ([ADR 0021](../../../docs/adr/0021-time-machine.md)).
pub const RETENTION_FOREVER: u64 = u64::MAX;

/// The retention window a cluster has when nothing has set one, in milliseconds: ten minutes.
///
/// Long enough that an ordinary transaction and an ordinary reader are never inside it, short
/// enough that a cluster nobody has configured does not keep every version it has ever written.
/// A cluster that has set one takes that instead — an absent record is *unconfigured*, not zero
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md)).
pub const DEFAULT_RETENTION_MS: u64 = 10 * 60 * 1_000;

/// The catalog's format version for a retention record.
const RETENTION_FORMAT_VERSION: u8 = 2;

/// Bits of the logical counter in a timestamp, so a retention in milliseconds becomes a
/// timestamp distance by a shift. Mirrors `esker_txn::TSO_LOGICAL_BITS`, which mirrors PD's.
const TSO_LOGICAL_BITS: u32 = esker_txn::TSO_LOGICAL_BITS;

/// How long each table keeps its old versions.
///
/// Loaded from the catalog's own records, which `esker-sql` writes and this crate only reads
/// ([ADR 0021](../../../docs/adr/0021-time-machine.md) decision 4):
///
/// ```text
/// 'm' ++ "sql" ++ 'd'                          the cluster default
/// 'm' ++ "sql" ++ 'r' ++ tenant:u64 ++ id:u64  one table's override
/// value, both: version:u8 = 2 ++ retention_ms:u64 little-endian
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPolicy {
    default_ms: u64,
    overrides: BTreeMap<(u64, u64), u64>,
}

impl RetentionPolicy {
    /// A policy with one window for everything and no overrides.
    #[must_use]
    pub fn uniform(default_ms: u64) -> Self {
        Self {
            default_ms,
            overrides: BTreeMap::new(),
        }
    }

    /// Records an override, for tests and for a caller building one by hand.
    #[must_use]
    pub fn with_table(mut self, tenant: u64, table_id: u64, retention_ms: u64) -> Self {
        self.overrides.insert((tenant, table_id), retention_ms);
        self
    }

    /// The cluster default.
    #[must_use]
    pub fn default_ms(&self) -> u64 {
        self.default_ms
    }

    /// How many tables have an override.
    #[must_use]
    pub fn overrides(&self) -> usize {
        self.overrides.len()
    }

    /// Reads the policy out of a database's catalog records.
    ///
    /// A record this build cannot read is a **typed error, never a guess**: guessing at a
    /// retention window is guessing at how much history to destroy. An *absent* default, by
    /// contrast, is not an error — a cluster that has never set one has the built-in default,
    /// which is what `fallback_ms` is.
    pub fn load(db: &Db, fallback_ms: u64) -> Result<Self, ProtoError> {
        let options = ReadOptions::default();
        let default_ms = match db
            .get(cf::DEFAULT, &default_key(), &options)
            .map_err(|error| engine_to_proto(&error))?
        {
            Some(bytes) => decode_retention(&bytes)?,
            None => fallback_ms,
        };

        // One prefix scan returns every override and nothing else, which is why the default
        // lives under `'d'` and not inside this range (ADR 0021 decision 4).
        let low = override_prefix();
        let mut high = low.clone();
        // The prefix's successor. The last byte is `'r'`, so this cannot carry.
        if let Some(last) = high.last_mut() {
            *last = last.saturating_add(1);
        }

        let mut overrides = BTreeMap::new();
        let mut iter = db
            .iter(cf::DEFAULT, &options)
            .map_err(|error| engine_to_proto(&error))?;
        iter.seek(&low);
        while iter.valid() && iter.key() < high.as_slice() {
            let (tenant, table_id) = split_override_key(iter.key())?;
            overrides.insert((tenant, table_id), decode_retention(iter.value())?);
            iter.next();
        }
        iter.status().map_err(|error| engine_to_proto(&error))?;

        Ok(Self {
            default_ms,
            overrides,
        })
    }

    /// The retention window for a table, or the cluster default when it has no override.
    ///
    /// A table with **no** override is not "retention zero" — it takes the default, which is the
    /// difference between an absent key and a key holding zero.
    #[must_use]
    pub fn retention_ms(&self, table: Option<(u64, u64)>) -> u64 {
        table
            .and_then(|id| self.overrides.get(&id).copied())
            .unwrap_or(self.default_ms)
    }

    /// The safepoint that applies to one engine key, given the one PD published.
    ///
    /// `None` means **collect nothing for this key**: either its table keeps everything for
    /// ever, or the arithmetic would have pushed the safepoint below zero, which is the same
    /// answer.
    #[must_use]
    pub fn effective_safepoint(&self, user_key: &[u8], published: u64) -> Option<u64> {
        let table = table_of(user_key);
        let retention_ms = self.retention_ms(table);
        if retention_ms == RETENTION_FOREVER {
            return None;
        }
        // An override may make a table keep *more*, never less: a longer retention pushes the
        // safepoint back by the difference, and a shorter one is discarded by the `min` until
        // PD learns to publish the cluster's smallest (ADR 0021).
        let Some(extra_ms) = retention_ms.checked_sub(self.default_ms) else {
            return Some(published);
        };
        let Some(extra) = extra_ms.checked_shl(TSO_LOGICAL_BITS) else {
            // A window so long its timestamp distance overflows keeps everything, which is the
            // honest reading of "longer than the whole history".
            return None;
        };
        published.checked_sub(extra)
    }
}

/// Deletes the spilled values nothing can ever name again, and answers how many went.
///
/// # Why this is a pass of its own and not part of the filter
///
/// A value longer than [`esker_txn::SHORT_VALUE_MAX_LEN`] is stored at **prewrite** in `default`
/// under `key::value(user_key, start_ts)`, and the `write` record's `start_ts` is the only link
/// back to it. `MvccCollector` is installed on `write` alone, so nothing ever collected these —
/// #60 measured `write` 96 → 8 against `default` 96 → 96 — and
/// [ADR 0111](../../../docs/adr/0111-a-deleted-keys-versions-are-dropped-as-one-segment.md) made
/// it sharper, because a deleted key's records now go entirely and the value loses its last
/// reference.
///
/// The filter cannot do it: a `CompactionFilter` returns a decision and `CompactionJob` writes to
/// one family's files, so there is no path from the `write` compaction to the `default` family.
/// Option (a) of [ADR 0112](../../../docs/adr/0112-collecting-a-spilled-value.md) is really a
/// durable queue out of the filter that has to survive a crash mid-compaction; this is option (b),
/// which needs no channel — a pass that dies half-way leaves work rather than damage.
///
/// # One snapshot, and that **is** the snapshot discipline
///
/// The rule ADR 0112 names is that the `write` family must be read at a snapshot no older than the
/// one `default` is walked at, and this states it the only way that cannot drift: **there is one
/// snapshot and both use it.** Arranging it by the order of two calls would be a rule that holds
/// until somebody moves a line.
///
/// # What is kept, and why each
///
/// An entry goes only when every one of these is false, because the failure direction here is
/// keeping a dead value and the other direction is losing a live one:
///
/// * **a surviving `write` record names its `start_ts`** — that link is how the read path resolves
///   a spilled value, so deleting it would read a committed row back as corruption;
/// * **the key still holds a lock** — the value lands at prewrite and the record that names it at
///   commit, so a transaction between the two has an entry nothing names *yet*. Deleting it there
///   loses a value that is about to be committed;
/// * **its `start_ts` is at or above the safepoint** — above the safepoint nothing is collectable
///   at all, and this pass has no business being the exception.
pub fn collect_spilled_values(db: &Db, safepoint: u64) -> Result<u64, ProtoError> {
    let pinned = db.snapshot();
    let options = ReadOptions {
        snapshot: Some(pinned.clone()),
        ..ReadOptions::default()
    };

    // Walked in key order, so every entry of one user key arrives together and the `write` scan
    // that judges them is one per key rather than one per entry.
    let mut orphans: Vec<Vec<u8>> = Vec::new();
    let mut iter = db
        .iter(cf::DEFAULT, &options)
        .map_err(|error| engine_to_proto(&error))?;
    let mut current: Option<(Vec<u8>, Vec<u64>, bool)> = None;
    iter.seek_to_first();
    while iter.valid() {
        let engine_key = iter.key().to_vec();
        // A key this pass does not understand belongs to someone else — `RawKv` pairs live in
        // `default` too — and is left alone.
        if let Ok((user_key, start_ts)) = key::split(&engine_key) {
            let named = match &current {
                Some((key, ..)) if key == &user_key => current.as_ref(),
                _ => {
                    current = Some((
                        user_key.clone(),
                        starts_named_by_write(db, &options, &user_key)?,
                        locked(db, &options, &user_key)?,
                    ));
                    current.as_ref()
                }
            };
            if let Some((_, named, locked)) = named
                && start_ts < safepoint
                && !*locked
                && !named.contains(&start_ts)
            {
                orphans.push(engine_key);
            }
        }
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    drop(iter);

    let count = orphans.len() as u64;
    for key in orphans {
        db.delete(cf::DEFAULT, &key)
            .map_err(|error| engine_to_proto(&error))?;
    }
    Ok(count)
}

/// Every `start_ts` a surviving `write` record of `user_key` names.
fn starts_named_by_write(
    db: &Db,
    options: &ReadOptions,
    user_key: &[u8],
) -> Result<Vec<u64>, ProtoError> {
    let (start, end) = key::version_range(user_key);
    let mut named = Vec::new();
    let mut iter = db
        .iter(cf::WRITE, options)
        .map_err(|error| engine_to_proto(&error))?;
    iter.seek(&start);
    while iter.valid() && iter.key() < end.as_slice() {
        if let Ok(record) = WriteRecord::decode(iter.value()) {
            named.push(record.start_ts);
        } else {
            // Unreadable: keep whatever it might have named, which is what every other decision in
            // this module does with corruption.
            return Ok(Vec::new());
        }
        iter.next();
    }
    iter.status().map_err(|error| engine_to_proto(&error))?;
    Ok(named)
}

/// Whether `user_key` still holds a lock — a transaction that has prewritten and not resolved.
fn locked(db: &Db, options: &ReadOptions, user_key: &[u8]) -> Result<bool, ProtoError> {
    db.get(cf::LOCK, &key::lock(user_key), options)
        .map(|found| found.is_some())
        .map_err(|error| engine_to_proto(&error))
}

/// The table an `'x'`-space key belongs to, if it belongs to one.
///
/// The engine key is `'x' ++ enc(user_key) ++ !ts`, so the user key has to come back out before
/// `esker-keys` can say which table it names — and a key that is not a table's (a `RawKV`-shaped
/// transactional key, say) simply has no override.
fn table_of(engine_key: &[u8]) -> Option<(u64, u64)> {
    let (user_key, _) = key::split(engine_key).ok()?;
    match prefix::split_table(&user_key) {
        Ok(Some((tenant, table_id, TablePart::Row | TablePart::Index))) => Some((tenant, table_id)),
        _ => None,
    }
}

fn default_key() -> Vec<u8> {
    prefix::meta_key(b"sqld")
}

fn override_prefix() -> Vec<u8> {
    prefix::meta_key(b"sqlr")
}

fn split_override_key(key: &[u8]) -> Result<(u64, u64), ProtoError> {
    let rest = key
        .strip_prefix(override_prefix().as_slice())
        .ok_or_else(|| {
            ProtoError::corrupt("retention record", "a key outside the override prefix")
        })?;
    if rest.len() != 16 {
        return Err(ProtoError::corrupt(
            "retention record",
            format!("an override key with {} bytes of ids, want 16", rest.len()),
        ));
    }
    let (tenant, rest) = esker_keys::codec::decode_u64(rest)
        .map_err(|error| ProtoError::corrupt("retention record", error.to_string()))?;
    let (table_id, _) = esker_keys::codec::decode_u64(rest)
        .map_err(|error| ProtoError::corrupt("retention record", error.to_string()))?;
    Ok((tenant, table_id))
}

/// `version:u8 = 2 ++ retention_ms:u64` little-endian, nine bytes.
///
/// An unknown version is a typed error and never a guess (`CLAUDE.md` invariant 2): a build
/// that read a record it does not understand would be deciding how much history to destroy from
/// bytes it cannot parse.
fn decode_retention(value: &[u8]) -> Result<u64, ProtoError> {
    if value.len() != 9 {
        return Err(ProtoError::corrupt(
            "retention record",
            format!("{} bytes, want 9", value.len()),
        ));
    }
    if value[0] != RETENTION_FORMAT_VERSION {
        return Err(ProtoError::corrupt(
            "retention record",
            format!(
                "catalog format version {}, this build reads {RETENTION_FORMAT_VERSION}",
                value[0]
            ),
        ));
    }
    let mut ms = [0u8; 8];
    ms.copy_from_slice(&value[1..]);
    Ok(u64::from_le_bytes(ms))
}

/// Drops the MVCC versions a retention policy has finished with.
///
/// # Why it holds state, and why the state is safe to share
///
/// "Keep the newest version below the safepoint" is a statement about a *run* of entries, not
/// about one — so the filter has to remember whether it has already kept one for the key it is
/// on. Compaction feeds it entries in sorted order, and within one user key the versions arrive
/// newest first (`enc_ts` is complemented), so one flag is enough.
///
/// The state records *which* version was kept, not merely that one was — and that is what makes
/// the filter **idempotent**. A key is compacted several times as it moves down the levels, and
/// a flag saying "already kept one" would drop the survivor on the second pass, which is
/// precisely how this went wrong the first time it was run. Recording the timestamp instead
/// means a later pass re-meets the version it kept, sees the same number, and keeps it again.
///
/// Two compactions may also run at once on one column family, over different key ranges, and
/// they share this filter. The state is guarded and keyed by the user key, so an interleaving
/// resets it and the filter **keeps a version it could have dropped**. That is the safe
/// direction and the only one: keeping too much costs space, dropping too much loses a row.
///
/// # Why it is installed once and updated in place
///
/// A compaction filter is configured when a column family is opened, and both of its inputs
/// move afterwards: PD publishes a new safepoint every few seconds, and a `retention` DDL
/// rewrites the policy. So the collector is a stable object holding both behind their own
/// locks, and the store updates it rather than reopening the database to change a number.
#[derive(Debug)]
pub struct MvccCollector {
    policy: RwLock<RetentionPolicy>,
    published: AtomicU64,
    seen: Mutex<Option<(Vec<u8>, u64)>>,
}

impl MvccCollector {
    /// A collector working to `published` under `policy`.
    #[must_use]
    pub fn new(policy: RetentionPolicy, published: u64) -> Self {
        Self {
            policy: RwLock::new(policy),
            published: AtomicU64::new(published),
            seen: Mutex::new(None),
        }
    }

    /// The policy it is working to.
    #[must_use]
    pub fn policy(&self) -> RetentionPolicy {
        self.policy.read().map_or_else(
            |poisoned| poisoned.into_inner().clone(),
            |policy| policy.clone(),
        )
    }

    /// Replaces the policy, after a `retention` DDL has changed one.
    pub fn set_policy(&self, policy: RetentionPolicy) {
        match self.policy.write() {
            Ok(mut held) => *held = policy,
            Err(poisoned) => *poisoned.into_inner() = policy,
        }
    }

    /// Raises the published safepoint. **Never lowers it**: a safepoint that moved backwards
    /// would promise a reader history that has already been collected, and PD's own only ever
    /// rises — so a lower number is a stale message overtaking a fresh one.
    pub fn set_published(&self, published: u64) -> u64 {
        self.published
            .fetch_max(published, Ordering::AcqRel)
            .max(published)
    }

    /// The safepoint it is working to.
    #[must_use]
    pub fn published(&self) -> u64 {
        self.published.load(Ordering::Acquire)
    }

    /// Whether this version is the newest at or below the safepoint for `user_key`, and so the
    /// one a read at the safepoint would return.
    ///
    /// Answers by *timestamp* rather than by a flag, so it gives the same answer however many
    /// times the same entry is offered — which happens, because a key is compacted again at
    /// every level it descends.
    ///
    /// Answers `true` — "keep it" — on a poisoned lock, because a collector that cannot tell
    /// what it has already kept must not decide that something is droppable.
    fn keep_as_newest(&self, user_key: &[u8], commit_ts: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return true;
        };
        match seen.as_mut() {
            Some((key, kept)) if key == user_key => {
                // Entries arrive newest first within a key, so the first is the largest. An
                // equal one is this version being offered again by a later pass.
                if commit_ts >= *kept {
                    *kept = commit_ts;
                    true
                } else {
                    false
                }
            }
            _ => {
                *seen = Some((user_key.to_vec(), commit_ts));
                true
            }
        }
    }
}

impl CompactionFilter for MvccCollector {
    fn filter(
        &self,
        _level: usize,
        engine_key: &[u8],
        value: &[u8],
        nothing_below: &dyn Fn(&[u8], &[u8]) -> bool,
    ) -> FilterDecision {
        // A key this collector does not understand is not its business. Every decision below
        // needs a `write` record and a version, and a key without them belongs to someone else.
        let Ok((user_key, commit_ts)) = key::split(engine_key) else {
            return FilterDecision::Keep;
        };
        let Ok(record) = WriteRecord::decode(value) else {
            // Corruption. Keeping it is what lets a reader report it rather than have it
            // silently collected — an unreadable record is evidence, not rubbish.
            return FilterDecision::Keep;
        };
        let Some(safepoint) = self
            .policy()
            .effective_safepoint(engine_key, self.published())
        else {
            // This table keeps everything.
            return FilterDecision::Keep;
        };

        // **A record that is not a version never claims the newest slot** (#78). `keep_as_newest`
        // answers "is this what a read at the safepoint returns", and a reader steps straight past
        // a rollback marker and a lock record looking for a version
        // (`esker_txn::percolator::newest_version_at`). One that went in would keep *itself* and
        // make the `Put` underneath it "older than the one we kept" — which collects the value and
        // leaves the key reading as absent. That is the P0 of run 127 attempt 4.
        if !record.kind.is_a_version() {
            // A rollback marker lives until the safepoint passes its `start_ts`, not its
            // `commit_ts` — they are the same number for a marker, but the rule is about the
            // transaction it kills, and below it a `Prewrite` from that transaction can still
            // arrive with nothing else to stop it (`docs/txn-spec.md` §7).
            //
            // A lock record has no such duty: it is a committed `Op::Check`, and nothing reads it
            // — the conflict check steps past it (ADR 0088) and so does the version walk. It goes
            // on its own `commit_ts`, at the same boundary the marker uses, because keeping one
            // record more is the side that costs space rather than rows.
            let bound = if record.kind == Kind::Rollback {
                record.start_ts
            } else {
                commit_ts
            };
            return if bound < safepoint {
                FilterDecision::Remove
            } else {
                FilterDecision::Keep
            };
        }

        if commit_ts > safepoint {
            // Some open read may still want it.
            return FilterDecision::Keep;
        }
        if self.keep_as_newest(&user_key, commit_ts) {
            // **The newest version, and what it says decides the whole segment** (ADR 0111).
            //
            // For a `Put` this is the value a read at the safepoint returns, so it stays. For a
            // `Delete` it is the *answer* "gone" — and keeping a record to say so keeps it for
            // ever: the key it sits on is never written again, so nothing merges it away and every
            // read of the range walks past it. #70's twelve-round probe measured twelve dropped
            // tables leaving ninety-six such records a round, each landing in a file of its own
            // that the bottom level never merges.
            //
            // Dropping it is the one collection decision that can **resurrect** a key, so it is
            // taken only where nothing older can survive it:
            //
            // * **the whole version span**, not this one key. The timestamp suffix is complemented
            //   (`esker_keys::codec::enc_ts`), so an older version sorts *after* the delete — a
            //   point query about the delete's own key answers "nothing below" while the older
            //   version is directly beneath it. `key::version_range` is the span and
            //   `nothing_below` is the engine's answer over it;
            // * **and everything under it is already collectable.** The entries arrive newest
            //   first, so every version after this one is older, and the engine only offers this
            //   filter entries at or below its own floor — the oldest live snapshot. A reader
            //   holding an older version pins the safepoint under the delete (ADR 0110 decision 1)
            //   and this branch is not reached at all, which is what
            //   `a_reader_below_the_delete_keeps_the_whole_segment` asserts.
            //
            // Composition with the rest of this filter: the segment rule only ever **narrows**.
            // A record outside the safepoint, an undecodable one, or a table that keeps everything
            // has already returned `Keep` above and never reaches here.
            if record.kind == Kind::Delete {
                let (start, end) = key::version_range(&user_key);
                if nothing_below(&start, &end) {
                    return FilterDecision::Remove;
                }
            }
            // The newest version at or below the safepoint: what a read at the safepoint
            // returns, so dropping it would make an existing key vanish.
            return FilterDecision::Keep;
        }
        FilterDecision::Remove
    }

    fn name(&self) -> &'static str {
        "esker.mvcc-collector"
    }
}

#[cfg(test)]
mod tests {
    use super::{MvccCollector, RETENTION_FOREVER, RetentionPolicy, decode_retention};

    /// **The aggressive answer**, which is the one a compaction that has reached the bottom gives
    /// and the one ADR 0111's segment rule acts on: no level below holds any version of the key.
    /// Saying `false` here would make every test below assert the *conservative* branch and never
    /// reach the rule.
    fn nothing_below(_start: &[u8], _end: &[u8]) -> bool {
        true
    }
    use esker_engine::compaction::{CompactionFilter, FilterDecision};
    use esker_keys::prefix;
    use esker_txn::codec::{Kind, WriteRecord};
    use esker_txn::key;

    /// A timestamp whose physical half is `ms`.
    fn ts(ms: u64) -> u64 {
        ms << esker_txn::TSO_LOGICAL_BITS
    }

    fn row_key(tenant: u64, table_id: u64, row: &[u8]) -> Vec<u8> {
        let mut out = prefix::table_row_prefix(tenant, table_id);
        out.extend_from_slice(row);
        out
    }

    /// An override may make a table keep **more**, never less.
    #[test]
    fn an_override_only_ever_pushes_the_safepoint_back() {
        let policy = RetentionPolicy::uniform(1_000)
            .with_table(1, 7, 3_000)
            .with_table(1, 8, 100);
        let published = ts(10_000);

        let longer = key::write(&row_key(1, 7, b"r"), 1);
        assert_eq!(
            policy.effective_safepoint(&longer, published),
            Some(ts(8_000)),
            "two seconds more retention is two seconds further back"
        );

        let shorter = key::write(&row_key(1, 8, b"r"), 1);
        assert_eq!(
            policy.effective_safepoint(&shorter, published),
            Some(published),
            "a shorter window gets no benefit yet; the min discards it"
        );

        let plain = key::write(&row_key(1, 9, b"r"), 1);
        assert_eq!(
            policy.effective_safepoint(&plain, published),
            Some(published),
            "no override is the cluster default, not retention zero"
        );
    }

    /// The sentinel. Subtracting it would underflow; the rule it stands for is "collect
    /// nothing".
    #[test]
    fn retention_forever_collects_nothing() {
        let policy = RetentionPolicy::uniform(1_000).with_table(1, 7, RETENTION_FOREVER);
        let key = key::write(&row_key(1, 7, b"r"), 1);
        assert_eq!(policy.effective_safepoint(&key, ts(10_000)), None);
    }

    /// A window longer than the whole history keeps everything rather than wrapping.
    #[test]
    fn a_window_older_than_the_cluster_keeps_everything() {
        let policy = RetentionPolicy::uniform(0).with_table(1, 7, 1_000_000);
        let key = key::write(&row_key(1, 7, b"r"), 1);
        assert_eq!(
            policy.effective_safepoint(&key, ts(10)),
            None,
            "the safepoint would go below zero, which is the same as collecting nothing"
        );
    }

    fn decide(
        collector: &MvccCollector,
        user_key: &[u8],
        commit_ts: u64,
        record: &WriteRecord,
    ) -> FilterDecision {
        collector.filter(
            0,
            &key::write(user_key, commit_ts),
            &record.encode(),
            &nothing_below,
        )
    }

    /// The rule of `docs/txn-spec.md` §7: below the safepoint, the newest version survives and
    /// everything older goes.
    #[test]
    fn the_newest_version_below_the_safepoint_survives() {
        let collector = MvccCollector::new(RetentionPolicy::uniform(0), 100);
        let k = b"k".to_vec();

        // Versions arrive newest first, which is what `enc_ts` guarantees.
        assert_eq!(
            decide(&collector, &k, 150, &WriteRecord::new(Kind::Put, 140)),
            FilterDecision::Keep,
            "above the safepoint: some open read may want it"
        );
        assert_eq!(
            decide(&collector, &k, 90, &WriteRecord::new(Kind::Put, 80)),
            FilterDecision::Keep,
            "the newest at or below it: what a read at the safepoint returns"
        );
        assert_eq!(
            decide(&collector, &k, 80, &WriteRecord::new(Kind::Put, 70)),
            FilterDecision::Remove
        );
        assert_eq!(
            decide(&collector, &k, 10, &WriteRecord::new(Kind::Put, 5)),
            FilterDecision::Remove
        );

        // A different key starts again: its own newest below the safepoint survives.
        assert_eq!(
            decide(&collector, b"other", 90, &WriteRecord::new(Kind::Put, 80)),
            FilterDecision::Keep
        );
    }

    /// A rollback marker outlives the ordinary rule: below its `start_ts` a `Prewrite` from
    /// that transaction can still arrive, and the marker is the only thing that stops it.
    #[test]
    fn a_rollback_marker_lives_until_the_safepoint_passes_its_start() {
        let collector = MvccCollector::new(RetentionPolicy::uniform(0), 100);
        // A marker sits at `commit_ts == start_ts`.
        assert_eq!(
            decide(&collector, b"k", 100, &WriteRecord::rollback(100)),
            FilterDecision::Keep,
            "at the safepoint, not past it"
        );
        assert_eq!(
            decide(&collector, b"k", 99, &WriteRecord::rollback(99)),
            FilterDecision::Remove,
            "the safepoint has passed it"
        );
    }

    /// A key the collector does not understand, and a record it cannot read, are both kept. An
    /// unreadable record is evidence rather than rubbish, and collecting it would destroy the
    /// only sign that something went wrong.
    #[test]
    fn what_it_cannot_read_it_keeps() {
        let collector = MvccCollector::new(RetentionPolicy::uniform(0), u64::MAX);
        assert_eq!(
            collector.filter(0, b"not a txn key", b"whatever", &nothing_below),
            FilterDecision::Keep
        );
        assert_eq!(
            collector.filter(0, &key::write(b"k", 1), b"\xff\xff\xff", &nothing_below),
            FilterDecision::Keep
        );
    }

    /// The record is behind the catalog's format version, and an unknown one is refused rather
    /// than guessed at — guessing here is guessing how much history to destroy.
    #[test]
    fn a_retention_record_this_build_cannot_read_is_refused() {
        let mut good = vec![2u8];
        good.extend_from_slice(&3_000u64.to_le_bytes());
        assert_eq!(decode_retention(&good).unwrap(), 3_000);

        let mut future = good.clone();
        future[0] = 3;
        assert!(decode_retention(&future).is_err(), "a newer format version");

        for cut in 0..good.len() {
            assert!(decode_retention(&good[..cut]).is_err(), "cut to {cut}");
        }
        let mut trailing = good;
        trailing.push(0);
        assert!(decode_retention(&trailing).is_err(), "a trailing byte");
    }
}
